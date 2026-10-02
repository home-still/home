#!/usr/bin/env bash
# Prove that release binaries are the architecture and the version they claim,
# BEFORE anything is published.
#
#   verify-binaries.sh --tag v0.0.1-rc.360 --target x86_64-unknown-linux-gnu \
#                      --dir <dir with the extracted binaries> [--run] <binary>...
#
# For every binary:
#   1. architecture  - `file` must report the target's CPU/format (skipped for
#                      Windows, where the --run below is the proof);
#   2. version marker - the binary must contain the framed marker
#                      `hs-version-marker:<version>:end` (build-support/
#                      version_marker.rs, included by all five binaries). A bare
#                      version string is NOT searched for: it is compiled into
#                      immediates and is not greppable. The marker works for
#                      binaries the runner cannot execute (cross-built targets)
#                      and for servers that have no --version flag;
#   3. --run         - additionally execute `<binary> --version` and require its
#                      last word to equal the version exactly. Pass --run only
#                      for binaries that have a clap `--version` and only when
#                      this runner can execute the target.
#
# rc.245 shipped rc.244's binary because nothing compared the binary with the
# tag. Any failure here exits non-zero and stops the release.
set -euo pipefail

die() { echo "verify-binaries: FAIL: $*" >&2; exit 1; }

tag= target= dir= run=false bins=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag=${2:?--tag needs a value}; shift 2 ;;
    --target) target=${2:?--target needs a value}; shift 2 ;;
    --dir) dir=${2:?--dir needs a value}; shift 2 ;;
    --run) run=true; shift ;;
    -*) die "unknown option $1" ;;
    *) bins+=("$1"); shift ;;
  esac
done
[ -n "$tag" ] && [ -n "$target" ] && [ -d "$dir" ] && [ ${#bins[@]} -gt 0 ] \
  || die "usage: verify-binaries.sh --tag <tag> --target <triple> --dir <dir> [--run] <binary>..."

[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "tag '$tag' is not vMAJOR.MINOR.PATCH[-PRERELEASE]"
version=${tag#v}

case "$target" in
  x86_64-unknown-linux-gnu)  suffix=;     arch_re='ELF 64-bit.*x86-64' ;;
  aarch64-unknown-linux-gnu) suffix=;     arch_re='ELF 64-bit.*(ARM aarch64|aarch64)' ;;
  x86_64-apple-darwin)       suffix=;     arch_re='Mach-O.*x86_64' ;;
  aarch64-apple-darwin)      suffix=;     arch_re='Mach-O.*arm64' ;;
  x86_64-pc-windows-msvc)    suffix=.exe; arch_re= ;;
  *) die "unknown target '$target'" ;;
esac

for bin in "${bins[@]}"; do
  path="$dir/$bin$suffix"
  [ -f "$path" ] || die "$path is missing"

  if [ -n "$arch_re" ]; then
    kind=$(file -b "$path")
    [[ "$kind" =~ $arch_re ]] || die "$bin is not a $target binary: file says '$kind'"
  elif ! $run; then
    die "$bin: no architecture check exists for $target, so --run is required"
  fi

  grep -a -F -q -- "hs-version-marker:${version}:end" "$path" \
    || die "$bin does not contain the version marker for '$version' (stale or wrongly built binary)"

  if $run; then
    out=$("$path" --version) || die "$bin --version exited non-zero"
    last=${out##*[[:space:]]}
    [ "$last" = "$version" ] || die "$bin --version printed '$out', expected version '$version'"
    echo "verify-binaries: ok  $target  $bin  ($out)"
  else
    echo "verify-binaries: ok  $target  $bin  (arch + version marker $version; not executed)"
  fi
done
