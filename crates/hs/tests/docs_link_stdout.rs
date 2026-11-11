//! `hs docs link` prints its URLs on **stdout**.
//!
//! The command exists to hand out a URL, and for a long time it printed
//! that URL through the reporter, which writes to stderr. Measured on
//! `big` 2026-11-11: `hs docs link <stem> > o.txt` left `o.txt` at 0
//! bytes and the URL in `e.txt`; `URL=$(hs docs link <stem>)` captured
//! nothing; `--quiet` printed nothing at all.
//!
//! A helper test cannot see which stream the bytes land on, so these
//! tests run the real binary.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

/// Run `hs` with `HOME` pointed at `home`.
///
/// A tempdir with no `~/.home-still/config.yaml` is a supported state:
/// the command creates the link key there.
fn run(args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(args)
        .env("HOME", home)
        // A release-profile build refuses to run without a tag; this
        // test is about a stream, not about versioning.
        .env_remove("HS_RELEASE_TAG")
        .output()
        .expect("running the hs binary")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr is UTF-8")
}

/// The one line a caller gets to pipe.
fn one_line(output: &Output) -> String {
    let out = stdout(output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one line on stdout, got {}: {out:?}",
        lines.len()
    );
    lines[0].to_string()
}

#[test]
fn the_url_is_on_stdout_and_nothing_else_is() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        &[
            "docs",
            "link",
            "--base-url",
            "http://host:7446",
            "10.1234_abcd",
        ],
        home.path(),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let url = one_line(&output);
    assert!(
        url.starts_with("http://host:7446/docs/v1/10.1234_abcd/source?"),
        "{url}"
    );
    assert!(url.contains("sig="), "{url}");

    // The human-facing line stays on stderr, and the URL does not.
    let err = stderr(&output);
    assert!(err.contains("Expires"), "{err}");
    assert!(
        !err.contains("http://"),
        "the URL was printed to stderr as well: {err}"
    );
}

#[test]
fn a_page_link_puts_the_page_in_both_the_query_and_the_fragment() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        &[
            "docs",
            "link",
            "--base-url",
            "http://host:7446",
            "--page",
            "3",
            "10.1234_abcd",
        ],
        home.path(),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let url = one_line(&output);
    // The query carries the page because the signature covers it; the
    // fragment carries it because the browser uses it to scroll.
    assert!(url.contains("&page=3&"), "{url}");
    assert!(url.ends_with("#page=3"), "{url}");
}

#[test]
fn the_remote_url_is_a_second_line() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".home-still/config.yaml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "docs:\n  lan_base_url: http://host:7446\n  remote_base_url: https://cloud.example\n",
    )
    .unwrap();

    let output = run(&["docs", "link", "--remote", "10.1234_abcd"], home.path());
    assert!(output.status.success(), "{}", stderr(&output));

    let out = stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected the LAN URL and the remote one: {out:?}"
    );
    assert!(
        lines[0].starts_with("http://host:7446/docs/v1/10.1234_abcd/source?"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("https://cloud.example/docs/v1/10.1234_abcd/source?"),
        "{}",
        lines[1]
    );
}

#[test]
fn quiet_still_prints_the_url() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        &[
            "--quiet",
            "docs",
            "link",
            "--base-url",
            "http://host:7446",
            "10.1234_abcd",
        ],
        home.path(),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // `--quiet` silences the reporter. The URL is the command's result,
    // not progress, so it survives.
    let url = one_line(&output);
    assert!(
        url.starts_with("http://host:7446/docs/v1/10.1234_abcd/source?"),
        "{url}"
    );
    assert_eq!(stderr(&output), "", "{}", stderr(&output));
}
