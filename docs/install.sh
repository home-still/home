#!/bin/sh
set -eu

REPO="home-still/home"
TOOL="hs"
INSTALL_DIR="${HOME}/.local/bin"
PRE=false

# Parse flags
for arg in "$@"; do
    case "$arg" in
        --pre) PRE=true ;;
        *)     echo "Usage: install.sh [--pre]"; exit 1 ;;
    esac
done

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *)      echo "Unsupported OS: ${OS}"; exit 1 ;;
esac

case "${ARCH}" in
    x86_64)         arch="x86_64" ;;
    arm64|aarch64)  arch="aarch64" ;;
    *)              echo "Unsupported architecture: ${ARCH}"; exit 1 ;;
esac

TARGET="${arch}-${os}"

# Get version from GitHub API (--pre includes release candidates)
if [ "$PRE" = true ]; then
    # Fetch all recent tags, extract versions, sort by semver, pick highest
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=10" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | sort -t. -k1,1n -k2,2n -k3,3n -k4,4n | tail -1)"
else
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
fi

if [ -z "${VERSION}" ]; then
    echo "Failed to fetch latest version"
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    echo "Neither sha256sum nor shasum found; cannot verify the download"
    exit 1
fi

mkdir -p "${INSTALL_DIR}"
# Staged inside INSTALL_DIR so the final mv is a same-filesystem rename: a
# running binary is replaced atomically, and a failed download or checksum
# leaves the previously installed binaries untouched.
WORK="$(mktemp -d "${INSTALL_DIR}/.install.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

# install_archive <binary> <required|optional>
# Downloads <binary>-<version>-<target>.tar.gz and its .sha256, verifies the
# archive against the digest, then installs the binary. An `optional` binary
# that the release does not publish for this platform (HTTP 404) is skipped;
# every other failure aborts the install.
install_archive() {
    bin="$1"
    archive="${bin}-${VERSION}-${TARGET}.tar.gz"
    base="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"

    code="$(curl -sSL -o "${WORK}/${archive}" -w '%{http_code}' "${base}")" \
        || { echo "Download of ${archive} failed"; exit 1; }
    if [ "${code}" = 404 ] && [ "$2" = optional ]; then
        return 0
    fi
    [ "${code}" = 200 ] || { echo "Download of ${archive} failed: HTTP ${code}"; exit 1; }

    code="$(curl -sSL -o "${WORK}/${archive}.sha256" -w '%{http_code}' "${base}.sha256")" \
        || { echo "Download of ${archive}.sha256 failed"; exit 1; }
    [ "${code}" = 200 ] || { echo "Download of ${archive}.sha256 failed: HTTP ${code}"; exit 1; }

    # `<64 hex>  <archive name>`
    expected="$(awk -v name="${archive}" '{ sub(/\r$/, "") } NF == 2 && $2 == name { print $1 }' "${WORK}/${archive}.sha256")"
    actual="$(sha256_of "${WORK}/${archive}")"
    if [ -z "${expected}" ] || [ "${expected}" != "${actual}" ]; then
        echo "Checksum mismatch for ${archive}: expected '${expected}', got '${actual}'"
        exit 1
    fi

    tar -xzf "${WORK}/${archive}" -C "${WORK}" "${bin}"
    chmod +x "${WORK}/${bin}"
    mv -f "${WORK}/${bin}" "${INSTALL_DIR}/${bin}"
    echo "Installed ${bin} to ${INSTALL_DIR}/${bin}"
}

echo "Installing ${TOOL} ${VERSION} for ${TARGET}..."
install_archive "${TOOL}" required

# Install companion binaries if available for this platform
for COMPANION in hs-distill-server hs-gateway hs-mcp; do
    install_archive "${COMPANION}" optional
done

# Check if INSTALL_DIR is in PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Add ${INSTALL_DIR} to your PATH:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc"
        ;;
esac
