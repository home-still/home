//! Test support: run one test of this binary in a child process.
//!
//! A hostile document is the one input that can end the whole process (a
//! stack overflow is an abort; `catch_unwind` cannot stop it), so the tests
//! that feed one to the code under test run it in a re-executed copy of the
//! test binary and assert on the child's exit status instead of risking the
//! harness. The child's entry point is an ordinary `#[test]` that does
//! nothing unless its environment variable is set.

/// What a child printed.
pub(crate) struct Child {
    pub stdout: String,
}

/// Re-execute this test binary running only `test_path` (e.g.
/// `epub::tests::child_entry`) with `env_key=env_value`. Panics — failing
/// the calling test — unless the child exits normally, naming how it died
/// otherwise (an abort shows as signal 6 / status 134).
pub(crate) fn run(test_path: &str, env_key: &str, env_value: &str) -> Child {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", test_path, "--nocapture", "--test-threads=1"])
        .env(env_key, env_value)
        .output()
        .expect("spawn the child test binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "the child process died ({:?}) on hostile input: {}",
        out.status,
        stderr.lines().rev().take(4).collect::<Vec<_>>().join(" | ")
    );
    Child { stdout }
}

/// Like [`run`] but for a child that is expected to die by its own hand:
/// returns its exit code (`None` = killed by a signal) and stdout.
pub(crate) fn run_expecting_exit(
    test_path: &str,
    env_key: &str,
    env_value: &str,
) -> (Option<i32>, String) {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", test_path, "--nocapture", "--test-threads=1"])
        .env(env_key, env_value)
        .output()
        .expect("spawn the child test binary");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

impl Child {
    /// The lines the child reported, each prefixed `RESULT ` (libtest prints
    /// `test <name> ... ` without a newline, so the first one shares its line
    /// with that prefix).
    pub(crate) fn results(&self) -> Vec<String> {
        self.stdout
            .lines()
            .filter_map(|line| line.find("RESULT ").map(|at| line[at + 7..].to_string()))
            .collect()
    }
}
