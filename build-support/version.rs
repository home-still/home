//! The one place `HS_VERSION` is derived.
//!
//! Every shipped binary (`hs`, `hs-gateway`, `hs-mcp`, `hs-scribe-server`,
//! `hs-distill-server`) bakes `HS_VERSION` into itself with
//! `env!("HS_VERSION")`. Each owning crate's `build.rs` is exactly
//!
//! ```ignore
//! #[path = "../../build-support/version.rs"]
//! mod version;
//! fn main() { version::emit(); }
//! ```
//!
//! so there is one derivation, not five copies that can drift.
//!
//! # Rules
//!
//! * `HS_RELEASE_TAG` set: the version is that tag without its leading `v`.
//!   The tag must look like `vMAJOR.MINOR.PATCH[-PRERELEASE]`; anything else
//!   (a branch name, an empty string, `v2-feature`) fails the build. CI sets
//!   it from the pushed tag; a local release build sets it by hand.
//! * `HS_RELEASE_TAG` unset, `PROFILE=release`: **the build fails.** A
//!   release-profile binary is a deployable binary and must name the tag it
//!   ships as. There is no `git describe` or `CARGO_PKG_VERSION` stand-in for
//!   one (the rc.245 incident shipped rc.244's binary that way).
//! * `HS_RELEASE_TAG` unset, any other profile: `git describe --tags
//!   --always` of the checkout (`0.0.1-rc.358-47-gd4a0d79`), re-evaluated
//!   whenever HEAD, the branch it points at, `packed-refs`, or a tag changes.
//!   If git cannot answer, the build fails.
//!
//! `CARGO_PKG_VERSION` is never consulted.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The tag the binary is being built to ship as, e.g. `v0.0.1-rc.360`.
pub const RELEASE_TAG_ENV: &str = "HS_RELEASE_TAG";

/// Emit `cargo:rustc-env=HS_VERSION=…` (and the rerun triggers), or print why
/// no version could be determined and fail the build.
pub fn emit() {
    match derive() {
        Ok(version) => println!("cargo:rustc-env=HS_VERSION={version}"),
        Err(why) => {
            eprintln!("\nerror: cannot determine HS_VERSION: {why}\n");
            std::process::exit(1);
        }
    }
}

fn derive() -> Result<String, String> {
    println!("cargo:rerun-if-env-changed={RELEASE_TAG_ENV}");
    let tag = match std::env::var(RELEASE_TAG_ENV) {
        Ok(tag) => Some(tag),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => return Err(format!("{RELEASE_TAG_ENV} is unusable: {e}")),
    };
    let profile =
        std::env::var("PROFILE").map_err(|e| format!("cargo did not provide PROFILE: {e}"))?;
    select_version(tag.as_deref(), &profile, describe_checkout)
}

/// The decision table, with the git lookup injected so it can be tested.
pub fn select_version(
    tag: Option<&str>,
    profile: &str,
    describe: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    match tag {
        Some(tag) => version_from_tag(tag),
        None if profile == "release" => Err(format!(
            "a release-profile build must name the tag it ships as, and {RELEASE_TAG_ENV} is not set.\n\
             Set it to the tag, e.g. `{RELEASE_TAG_ENV}=v0.0.1-rc.360 cargo build --release -p hs`\n\
             (CI sets it from the pushed tag; with docker pass `--build-arg {RELEASE_TAG_ENV}=…`).\n\
             A development binary (no `--release`) derives its version from `git describe` instead."
        )),
        None => describe().and_then(|raw| version_from_describe(&raw)),
    }
}

/// `v0.0.1-rc.360` -> `0.0.1-rc.360`; rejects anything that is not a release tag.
pub fn version_from_tag(tag: &str) -> Result<String, String> {
    let bad = || {
        let empty = if tag.is_empty() {
            format!(" It is set but empty (docker needs `--build-arg {RELEASE_TAG_ENV}=vMAJOR.MINOR.PATCH…`).")
        } else {
            String::new()
        };
        format!(
            "{RELEASE_TAG_ENV}={tag:?} is not a release tag; expected \
             vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-PRERELEASE (for example v0.0.1-rc.360).{empty}"
        )
    };
    let version = tag.strip_prefix('v').ok_or_else(bad)?;
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let core_ok = {
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() == 3 && parts.iter().all(|p| is_numeric_identifier(p))
    };
    let pre_ok = pre.is_none_or(|pre| {
        pre.split('.').all(|id| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                // semver: an all-digit identifier has no leading zero
                && (!id.bytes().all(|b| b.is_ascii_digit()) || is_numeric_identifier(id))
        })
    });
    if core_ok && pre_ok {
        Ok(version.to_string())
    } else {
        Err(bad())
    }
}

fn is_numeric_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'))
}

/// `v0.0.1-rc.358-47-gd4a0d79\n` -> `0.0.1-rc.358-47-gd4a0d79`.
pub fn version_from_describe(raw: &str) -> Result<String, String> {
    let described = raw.trim();
    let version = described.strip_prefix('v').unwrap_or(described);
    if version.is_empty() || version.chars().any(char::is_whitespace) {
        return Err(format!("`git describe` returned {raw:?}"));
    }
    Ok(version.to_string())
}

fn describe_checkout() -> Result<String, String> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("cargo did not provide CARGO_MANIFEST_DIR")?,
    );
    let (git_dir, common_dir) = git_dirs(&manifest_dir)?;
    for path in rerun_paths(&git_dir, &common_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    git(&manifest_dir, &["describe", "--tags", "--always"])
}

/// The per-worktree git dir (holds `HEAD`) and the common git dir (holds
/// `refs/` and `packed-refs`). They differ in a linked worktree, where `.git`
/// is a *file* and the real directory is `<repo>/.git/worktrees/<name>`.
fn git_dirs(manifest_dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let out = git(
        manifest_dir,
        &["rev-parse", "--git-dir", "--git-common-dir"],
    )?;
    let mut lines = out.lines();
    let (Some(git_dir), Some(common_dir), None) = (lines.next(), lines.next(), lines.next()) else {
        return Err(format!("unexpected `git rev-parse` output {out:?}"));
    };
    // Relative answers are relative to the directory git ran in.
    Ok((manifest_dir.join(git_dir), manifest_dir.join(common_dir)))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let hint = format!(
        "build inside a git checkout, or set {RELEASE_TAG_ENV}=vMAJOR.MINOR.PATCH[-PRERELEASE]"
    );
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run `git {}`: {e}; {hint}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "`git {}` failed ({}): {}; {hint}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout)
        .map_err(|e| format!("`git {}` printed non-UTF-8: {e}", args.join(" ")))
}

/// Everything `git describe --tags --always` depends on that a commit,
/// checkout, rebase, tag or `git gc` can change. Only paths that exist are
/// returned (cargo treats a missing `rerun-if-changed` path as always stale).
pub fn rerun_paths(git_dir: &Path, common_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![git_dir.join("HEAD")];

    // The branch HEAD points at moves on every commit. A packed branch has no
    // loose file until its first commit after packing, so watch the nearest
    // existing directory where that file will appear.
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        if let Some(branch) = head.trim().strip_prefix("ref:").map(str::trim) {
            let mut candidate = common_dir.join(branch);
            while !candidate.exists() && candidate != common_dir {
                if !candidate.pop() {
                    break;
                }
            }
            paths.push(candidate);
        }
    }

    // Tags: a new loose tag is a new file under refs/tags; gc moves them into
    // packed-refs.
    for extra in ["packed-refs", "refs/tags", "reftable"] {
        let path = common_dir.join(extra);
        if path.exists() {
            paths.push(path);
        }
    }
    paths.retain(|p| p.exists());
    paths
}
