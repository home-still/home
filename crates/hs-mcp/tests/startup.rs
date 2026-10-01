//! Runs the real `hs-mcp` binary against a throwaway HOME. Nothing is
//! served: every case stops during startup, which is what is asserted.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

const TOKEN_VAR: &str = "HS_BACKEND_TOKEN";
const TOKEN: &str = "0123456789abcdef0123456789abcdef-secret-from-file";

fn home_with(config: Option<&str>, secrets: Option<&str>) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".home-still");
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(config) = config {
        std::fs::write(dir.join("config.yaml"), config).unwrap();
    }
    if let Some(secrets) = secrets {
        std::fs::write(dir.join("secrets.env"), secrets).unwrap();
    }
    home
}

fn run_http(home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hs-mcp"))
        .args(["--serve", "127.0.0.1:0"])
        .env_clear()
        .env("HOME", home)
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn without_a_token_the_server_refuses_to_start_naming_the_variable() {
    let home = home_with(None, None);
    let out = run_http(home.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains(TOKEN_VAR), "{}", stderr(&out));
}

#[test]
fn a_token_that_only_secrets_env_provides_is_seen_by_the_startup_that_follows() {
    // RA-81: secrets.env is loaded before the runtime and before the token
    // is read. With the token present the token check passes and startup
    // goes on to the next requirement (a `storage:` section), proving the
    // order; the token check is the first thing after the secrets load.
    let home = home_with(None, Some(&format!("{TOKEN_VAR}={TOKEN}\n")));
    let out = run_http(home.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(!err.contains("without a backend token"), "{err}");
    assert!(err.contains("storage"), "{err}");
    assert!(
        !err.contains(TOKEN),
        "the secret must never be printed: {err}"
    );
}

#[test]
fn an_unreadable_secrets_file_is_fatal_not_ignored() {
    // A directory where the file should be: opening succeeds, reading fails.
    let home = home_with(None, None);
    std::fs::create_dir(home.path().join(".home-still/secrets.env")).unwrap();
    let out = run_http(home.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("secrets.env"), "{}", stderr(&out));
}

#[test]
fn a_malformed_config_section_stops_the_server_naming_the_section() {
    for (config, needle) in [
        ("storage:\n  backend: carrier-pigeon\n", "storage"),
        ("logs:\n  ship_interval_secs: soon\n", "logs"),
        ("storage: {backend: local\n", "config.yaml"),
    ] {
        let home = home_with(Some(config), Some(&format!("{TOKEN_VAR}={TOKEN}\n")));
        let out = run_http(home.path());
        assert!(!out.status.success(), "{config}");
        assert!(stderr(&out).contains(needle), "{config}: {}", stderr(&out));
    }
}

#[test]
fn a_missing_events_section_stops_the_server_instead_of_dropping_publishes() {
    let home = home_with(
        Some("storage:\n  backend: local\n  local:\n    root: /nonexistent-hs-root-for-test\n"),
        Some(&format!("{TOKEN_VAR}={TOKEN}\n")),
    );
    let out = run_http(home.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    // Either the storage root or the bus is the first thing refused; the bus
    // case is exercised once the storage root exists.
    assert!(err.contains("storage") || err.contains("events"), "{err}");

    let root = home.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let config = format!(
        "storage:\n  backend: local\n  local:\n    root: {}\n",
        root.display()
    );
    std::fs::write(home.path().join(".home-still/config.yaml"), config).unwrap();
    let out = run_http(home.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("events"), "{}", stderr(&out));
}
