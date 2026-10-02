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
    let home = home_with(None, Some(&format!("{TOKEN_VAR}={TOKEN}\n")));
    let root = home.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    write_config(&home, &format!("{}\n", storage_section(&root)));
    let out = run_http(home.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("events.backend"), "{}", stderr(&out));
}

fn storage_section(root: &Path) -> String {
    format!(
        "storage:\n  backend: local\n  local:\n    root: {}\n",
        root.display()
    )
}

fn write_config(home: &tempfile::TempDir, config: &str) {
    std::fs::write(home.path().join(".home-still/config.yaml"), config).unwrap();
}

#[test]
fn an_invalid_events_section_still_stops_the_server_at_startup() {
    // The broker connection is deferred; the configuration is not.
    let root_home = home_with(None, Some(&format!("{TOKEN_VAR}={TOKEN}\n")));
    let root = root_home.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    for (events, needle) in [
        ("events:\n  backend: carrier-pigeon\n", "events"),
        (
            "events:\n  backend: nats\n  nats:\n    user: only-a-user\n",
            "go together",
        ),
        (
            "events:\n  backend: nats\n  nats:\n    token_env: HS_TEST_NATS_TOKEN_NOT_SET\n",
            "HS_TEST_NATS_TOKEN_NOT_SET",
        ),
        (
            "events:\n  backend: nats\n  nats:\n    credentials_file: /nonexistent/hs.creds\n",
            "credentials_file",
        ),
    ] {
        write_config(&root_home, &format!("{}{events}", storage_section(&root)));
        let out = run_http(root_home.path());
        assert!(!out.status.success(), "{events}");
        assert!(stderr(&out).contains(needle), "{events}: {}", stderr(&out));
    }
}

/// N6: with the broker down the server starts and stays up (the old
/// behaviour was a restart loop that took every read-only tool with it).
#[test]
fn an_unreachable_broker_does_not_stop_the_server_from_starting() {
    use std::io::{Read, Write};

    let home = home_with(None, Some(&format!("{TOKEN_VAR}={TOKEN}\n")));
    let root = home.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    // A closed loopback port is the broker.
    write_config(
        &home,
        &format!(
            "{}events:\n  backend: nats\n  nats:\n    url: nats://127.0.0.1:1\n",
            storage_section(&root)
        ),
    );
    // Port 0, read back from the server's log: a port chosen by the test and
    // released before the server binds it can be taken in between (seen under
    // load as `Address already in use`, or as a connection to another process).
    let mut child = Command::new(env!("CARGO_BIN_EXE_hs-mcp"))
        .args(["--serve", "127.0.0.1:0"])
        .env_clear()
        .env("HOME", home.path())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    let log = std::thread::spawn(move || {
        let mut seen = String::new();
        for line in std::io::BufRead::lines(std::io::BufReader::new(stderr)).map_while(Result::ok) {
            if let Some(addr) = line.split("MCP server listening on ").nth(1) {
                let _ = tx.send(addr.trim().to_string());
            }
            seen.push_str(&line);
            seen.push('\n');
        }
        seen
    });
    // Disconnected (the server exited, closing stderr) or timed out.
    let Ok(addr) = rx.recv_timeout(std::time::Duration::from_secs(30)) else {
        let _ = child.kill();
        let status = child.wait().unwrap();
        panic!(
            "the server never started listening ({status}) with the broker down: {}",
            log.join().unwrap()
        );
    };

    let mut conn = std::net::TcpStream::connect(&addr).unwrap();
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    conn.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).unwrap();
    let still_running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    let log = log.join().unwrap();
    assert!(still_running, "the server exited after answering: {log}");
    // Up, and answering (unauthenticated requests get 401): the broker is
    // only needed once a tool publishes.
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}\n{log}");
}
