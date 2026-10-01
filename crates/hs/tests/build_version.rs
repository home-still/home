//! `build-support/version.rs` is compiled into the `build.rs` of every crate
//! that ships a binary, where `cargo test` cannot reach it. Including it here
//! as a module tests the exact source the build scripts run.
#![allow(dead_code)]

#[path = "../../../build-support/version.rs"]
mod version;

use version::{rerun_paths, select_version, version_from_describe, version_from_tag};

fn no_git() -> Result<String, String> {
    panic!("git must not be consulted when the tag decides the version")
}

#[test]
fn a_release_tag_is_the_version_without_its_v() {
    assert_eq!(version_from_tag("v0.0.1-rc.360").unwrap(), "0.0.1-rc.360");
    assert_eq!(version_from_tag("v1.2.3").unwrap(), "1.2.3");
    assert_eq!(version_from_tag("v0.0.0").unwrap(), "0.0.0");
    assert_eq!(
        version_from_tag("v10.20.30-alpha-1.x.7").unwrap(),
        "10.20.30-alpha-1.x.7"
    );
}

#[test]
fn only_release_tags_are_accepted() {
    for bad in [
        "",
        "v",
        "main",
        "refs/tags/v0.0.1",
        // a branch that happens to start with `v` (GITHUB_REF_NAME on a push to it)
        "v2-feature",
        "0.0.1-rc.5",
        "V0.0.1",
        "v1",
        "v1.2",
        "v1.2.3.4",
        "v1.2.x",
        "v01.2.3",
        "v1.2.3-",
        "v1.2.3-rc..1",
        "v1.2.3-rc.01",
        "v1.2.3+build",
        "v1.2.3-rc.1 ",
        "v1.2.3-rc.1\n",
    ] {
        let err = version_from_tag(bad).expect_err(bad);
        assert!(err.contains("HS_RELEASE_TAG"), "{bad:?}: {err}");
    }
}

#[test]
fn every_tag_this_project_has_shipped_parses_as_semver() {
    // upgrade_cmd parses release tags with the semver crate; the build must
    // not bake something `hs upgrade` would then misread.
    for tag in ["v0.0.1-rc.39", "v0.0.1-rc.245", "v0.0.1-rc.358"] {
        let baked = version_from_tag(tag).unwrap();
        let parsed = semver::Version::parse(&baked).unwrap();
        assert_eq!(parsed.to_string(), baked);
    }
}

#[test]
fn an_explicit_tag_decides_in_every_profile_without_asking_git() {
    for profile in ["release", "debug"] {
        assert_eq!(
            select_version(Some("v0.0.1-rc.360"), profile, no_git).unwrap(),
            "0.0.1-rc.360"
        );
    }
}

#[test]
fn a_bad_tag_does_not_fall_back_to_git() {
    for profile in ["release", "debug"] {
        let err = select_version(Some("main"), profile, no_git).unwrap_err();
        assert!(err.contains("not a release tag"), "{err}");
    }
}

#[test]
fn a_release_build_without_a_tag_fails_instead_of_guessing() {
    let err = select_version(None, "release", no_git).unwrap_err();
    assert!(err.contains("HS_RELEASE_TAG is not set"), "{err}");
    assert!(
        err.contains("--build-arg"),
        "docker users need the remedy: {err}"
    );
}

#[test]
fn a_development_build_describes_the_checkout() {
    let v = select_version(None, "debug", || Ok("v0.0.1-rc.358-47-gd4a0d79\n".into())).unwrap();
    assert_eq!(v, "0.0.1-rc.358-47-gd4a0d79");
    // No tag reachable (shallow CI checkout): git's `--always` answer is a bare sha.
    assert_eq!(version_from_describe("d4a0d79\n").unwrap(), "d4a0d79");
}

#[test]
fn a_development_build_that_git_cannot_describe_fails() {
    let err = select_version(None, "debug", || Err("not a git repository".into())).unwrap_err();
    assert_eq!(err, "not a git repository");
    assert!(version_from_describe("").is_err());
    assert!(version_from_describe(" \n").is_err());
    assert!(version_from_describe("two words").is_err());
}

/// A linked worktree: `.git` is a file; HEAD lives in `<repo>/.git/worktrees/<n>`
/// while branches, tags and packed-refs live in `<repo>/.git`.
struct Layout {
    _dir: tempfile::TempDir,
    git_dir: std::path::PathBuf,
    common_dir: std::path::PathBuf,
}

fn layout(head: &str) -> Layout {
    let dir = tempfile::tempdir().unwrap();
    let common_dir = dir.path().join("repo/.git");
    let git_dir = common_dir.join("worktrees/ci");
    std::fs::create_dir_all(common_dir.join("refs/heads")).unwrap();
    std::fs::create_dir_all(common_dir.join("refs/tags")).unwrap();
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), head).unwrap();
    Layout {
        _dir: dir,
        git_dir,
        common_dir,
    }
}

#[test]
fn a_commit_on_the_current_branch_retriggers_the_build_script() {
    let l = layout("ref: refs/heads/work\n");
    std::fs::write(l.common_dir.join("refs/heads/work"), "0123\n").unwrap();
    std::fs::write(l.common_dir.join("packed-refs"), "# pack-refs\n").unwrap();

    let paths = rerun_paths(&l.git_dir, &l.common_dir);
    assert!(paths.contains(&l.git_dir.join("HEAD")), "{paths:?}");
    assert!(
        paths.contains(&l.common_dir.join("refs/heads/work")),
        "the branch ref (in the COMMON dir, not the worktree dir): {paths:?}"
    );
    assert!(
        paths.contains(&l.common_dir.join("packed-refs")),
        "{paths:?}"
    );
    assert!(paths.contains(&l.common_dir.join("refs/tags")), "{paths:?}");
}

#[test]
fn a_packed_branch_is_watched_where_its_loose_ref_will_appear() {
    let l = layout("ref: refs/heads/ra/ci\n");
    // refs/heads/ra does not exist yet: the nearest existing ancestor is watched.
    let paths = rerun_paths(&l.git_dir, &l.common_dir);
    assert!(
        paths.contains(&l.common_dir.join("refs/heads")),
        "{paths:?}"
    );

    std::fs::create_dir_all(l.common_dir.join("refs/heads/ra")).unwrap();
    let paths = rerun_paths(&l.git_dir, &l.common_dir);
    assert!(
        paths.contains(&l.common_dir.join("refs/heads/ra")),
        "{paths:?}"
    );
}

#[test]
fn a_detached_head_watches_only_head_and_the_tags() {
    let l = layout("d4a0d79d4a0d79d4a0d79d4a0d79d4a0d79d4a0d\n");
    let paths = rerun_paths(&l.git_dir, &l.common_dir);
    assert_eq!(
        paths,
        vec![l.git_dir.join("HEAD"), l.common_dir.join("refs/tags")]
    );
}

#[test]
fn only_existing_paths_are_ever_watched() {
    // cargo treats a missing rerun-if-changed path as permanently stale, which
    // would rebuild every dependent crate on every invocation.
    let l = layout("ref: refs/heads/work\n");
    for p in rerun_paths(&l.git_dir, &l.common_dir) {
        assert!(p.exists(), "{}", p.display());
    }
}
