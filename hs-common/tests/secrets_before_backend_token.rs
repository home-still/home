//! A backend token that exists only in `secrets.env` must be visible to
//! `BackendToken::from_env` once `load_secrets_from` has run, which is the
//! order every server's `main()` now follows (secrets, then runtime, then the
//! token read). Only the first test touches the process environment, and it
//! is the only one that reads it, so they cannot interfere.
#![cfg(feature = "auth")]

use hs_common::auth::backend::{BackendToken, ENV_VAR};

#[test]
fn a_token_provided_only_via_secrets_env_is_visible_to_from_env() {
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
}

#[test]
fn an_unreadable_secrets_file_is_an_error_the_caller_can_propagate() {
    // A directory where the file should be: reading it fails.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.env");
    std::fs::create_dir(&path).unwrap();
    assert!(hs_common::secrets::load_secrets_from(&path).is_err());
}
