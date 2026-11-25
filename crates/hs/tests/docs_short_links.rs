//! `hs docs link --short` mints a code, and `hs docs prune` deletes the
//! expired ones.
//!
//! The code is a record in the corpus, and a record the caller is shown but
//! the service cannot resolve is a 404 the user finds instead of the
//! document. So these tests read the record back out of the storage root the
//! command was pointed at, rather than trusting what was printed.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `hs` with `HOME` pointed at `home`.
fn run(args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hs"))
        .args(args)
        .env("HOME", home)
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

/// A home with a `docs:` and a `storage:` section, and the corpus root.
///
/// The corpus root is a temporary directory: a test that wrote into the real
/// one would leave records behind, and `hs docs prune` would then delete
/// somebody's live code.
fn home_with_corpus(tmp: &Path) -> PathBuf {
    let corpus = tmp.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    let config = tmp.join(".home-still/config.yaml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!(
            "docs:\n  lan_base_url: http://host:7446\nstorage:\n  backend: local\n  local:\n    \
             root: {}\n",
            corpus.display()
        ),
    )
    .unwrap();
    corpus
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

/// Put a short-link record in the corpus, as an earlier run would have.
fn put_record(corpus: &Path, code: &str, expires_at: u64) {
    let dir = corpus.join("short-links");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{code}.json")),
        format!(
            r#"{{"target":"/docs/v1/10.1234_abcd/source?exp={expires_at}","expires_at":{expires_at}}}"#
        ),
    )
    .unwrap();
}

#[test]
fn a_short_link_is_printed_and_stored_in_the_corpus() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = home_with_corpus(tmp.path());

    let output = run(&["docs", "link", "--short", "10.1234_abcd"], tmp.path());
    assert!(output.status.success(), "{}", stderr(&output));

    // What is printed is the code URL, not the signed one.
    let url = one_line(&output);
    let code = url
        .strip_prefix("http://host:7446/s/")
        .unwrap_or_else(|| panic!("expected a short link, got {url}"));

    // What was printed is what was stored: the code resolves to the signed
    // path the long link would have carried.
    let record = std::fs::read_to_string(corpus.join(format!("short-links/{code}.json")))
        .unwrap_or_else(|e| panic!("the code {code} that was printed has no record: {e}"));
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    let target = record["target"].as_str().unwrap();
    assert!(
        target.starts_with("/docs/v1/10.1234_abcd/source?"),
        "the redirect must land on the signed path: {target}"
    );
    assert!(target.contains("sig="), "{target}");
    assert!(
        record["expires_at"].as_u64().unwrap() > 0,
        "a code needs the expiry of the link behind it, or prune cannot prune"
    );
    // The target is a path, so the same code works on the LAN and away.
    assert!(!target.contains("://"), "{target}");
}

#[test]
fn a_page_short_link_carries_the_fragment_the_redirect_cannot_re_add() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = home_with_corpus(tmp.path());

    let output = run(
        &["docs", "link", "--short", "--page", "3", "10.1234_abcd"],
        tmp.path(),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let url = one_line(&output);
    let code = url.strip_prefix("http://host:7446/s/").unwrap();
    let record = std::fs::read_to_string(corpus.join(format!("short-links/{code}.json"))).unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();

    // A browser following a `Location` has already lost any fragment the
    // client had, so the code has to store one.
    assert_eq!(record["fragment"], "#page=3");
    assert!(
        record["target"].as_str().unwrap().contains("page=3"),
        "{}",
        record["target"]
    );
}

#[test]
fn prune_deletes_the_expired_codes_and_keeps_the_live_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = home_with_corpus(tmp.path());
    put_record(&corpus, "aaaaaaaaaaaa", u64::MAX);
    put_record(&corpus, "bbbbbbbbbbbb", 1);

    let output = run(&["docs", "prune"], tmp.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("1 expired short link"),
        "{}",
        stderr(&output)
    );

    assert!(
        corpus.join("short-links/aaaaaaaaaaaa.json").exists(),
        "prune must never delete a code that still works"
    );
    assert!(
        !corpus.join("short-links/bbbbbbbbbbbb.json").exists(),
        "an expired code is a redirect into a 403, so it is garbage"
    );
}

#[test]
fn a_short_link_without_a_storage_section_is_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join(".home-still/config.yaml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "docs:\n  lan_base_url: http://host:7446\n").unwrap();

    let output = run(&["docs", "link", "--short", "10.1234_abcd"], tmp.path());
    assert!(!output.status.success(), "{}", stdout(&output));

    // A code that is printed but not stored is a 404, so the command fails
    // rather than printing one — and it names what is missing.
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(stderr(&output).contains("storage:"), "{}", stderr(&output));
}
