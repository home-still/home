//! A backend token that exists only in `secrets.env` must be visible to
//! `BackendToken::from_env` once `load_secrets_from` has run, which is the
//! order every server's `main()` now follows (secrets, then runtime, then the
//! token read). This binary holds a single test: it mutates the process
//! environment, and a concurrent test thread reading it (`tempdir()`,
//! `var_os`) beside `setenv` can SIGSEGV on glibc before 2.41.
#![cfg(feature = "auth")]

use hs_common::auth::backend::{BackendToken, ENV_VAR};

#[test]
fn secrets_env_feeds_the_token_read_and_an_unreadable_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.env");
    std::fs::write(
        &path,
        format!("{ENV_VAR}=0123456789abcdef0123456789abcdef\n"),
    )
    .unwrap();

    // SAFETY: the only test in this binary; no other thread reads the env.
    unsafe { std::env::remove_var(ENV_VAR) };
    let before = BackendToken::from_env().expect_err("not set yet");
    assert!(format!("{before:#}").contains(ENV_VAR));

    assert_eq!(hs_common::secrets::load_secrets_from(&path).unwrap(), 1);
    BackendToken::from_env().expect("loaded from secrets.env");

    // A directory where the file should be: reading it fails.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.env");
    std::fs::create_dir(&path).unwrap();
    assert!(hs_common::secrets::load_secrets_from(&path).is_err());
}
