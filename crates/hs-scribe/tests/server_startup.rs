//! Runs the real `hs-scribe-server` binary against a throwaway HOME. The
//! server must refuse to start on a bad configuration instead of running on
//! defaults. No port is bound and no model is loaded in these cases.
#![cfg(all(feature = "server", unix))]

use std::process::{Command, Output};

fn run(config: &str, env: &[(&str, &str)]) -> Output {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".home-still");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hs-scribe-server"));
    cmd.args(["--host", "127.0.0.1", "--port", "0"])
        .env_clear()
        .env("HOME", home.path());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_malformed_scribe_server_section_refuses_to_start_naming_the_key() {
    for (config, needle) in [
        ("scribe_server:\n  vlm_concurrency: lots\n", "scribe_server"),
        ("scribe_server:\n  vlm_concurrency: 0\n", "vlm_concurrency"),
        ("scribe_server:\n  backend: sglang\n", "scribe_server"),
        ("storage:\n  backend: carrier-pigeon\n", "storage"),
        ("logs:\n  ship_interval_secs: 0\n", "ship_interval_secs"),
    ] {
        let out = run(config, &[]);
        assert!(!out.status.success(), "{config}");
        assert!(stderr(&out).contains(needle), "{config}: {}", stderr(&out));
    }
}

#[test]
fn a_bad_environment_override_refuses_to_start() {
    let out = run("", &[("HS_SCRIBE_VLM_CONCURRENCY", "many")]);
    assert!(!out.status.success());
    assert!(
        stderr(&out)
            .to_ascii_lowercase()
            .contains("vlm_concurrency"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_server_without_the_backend_token_refuses_to_start() {
    // A valid (empty) configuration: the token is the only thing missing.
    let out = run("", &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("HS_BACKEND_TOKEN"), "{err}");

    let out = run("", &[("HS_BACKEND_TOKEN", "too-short")]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("HS_BACKEND_TOKEN") && !err.contains("too-short"),
        "{err}"
    );
}

#[test]
fn an_olmocr_server_without_its_cli_refuses_to_start() {
    // The CLI is checked before the token, libpdfium or anything else.
    let out = run(
        "",
        &[
            ("HS_SCRIBE_CONVERTER", "olmocr"),
            ("HS_SCRIBE_OLMOCR_BIN", "/nonexistent/olmocr"),
        ],
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("olmocr_bin"), "{err}");
}
