//! `Config::load_from` against a config file under a hermetic process
//! environment plus per-test variables (figment's `Jail`, which serialises
//! the tests that touch the environment).
//!
//! These live in their own test binary on purpose. `setenv` reallocates the
//! `environ` array, and on glibc before 2.41 a concurrent `getenv` from any
//! other thread (a `tempdir()`, `dirs::home_dir()`) reads freed memory and
//! can SIGSEGV. In the lib test binary unrelated tests run beside the ones
//! that set variables; here every test holds the `Jail` lock.

use hs_common::config_file::ConfigFile;
use personal::config::Config;
use std::path::{Path, PathBuf};

/// Load against a config file with `yaml` as its content (or none) under
/// a hermetic environment plus `vars`.
#[allow(clippy::result_large_err)] // figment's `Jail` closure type
fn load_yaml(yaml: Option<&str>, vars: &[(&str, &str)]) -> personal::error::Result<Config> {
    let mut out = None;
    figment::Jail::expect_with(|jail| {
        jail.clear_env();
        for (k, v) in vars {
            jail.set_env(k, v);
        }
        let home = jail.directory().to_path_buf();
        if let Some(yaml) = yaml {
            let path = home.join(hs_common::CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, yaml).unwrap();
        }
        let user = ConfigFile::load_in(&home).unwrap();
        let system = ConfigFile::load_at(Path::new("/nonexistent/config.yaml"), &home).unwrap();
        out = Some(Config::load_from(&system, &user));
        Ok(())
    });
    out.expect("closure ran")
}

#[test]
fn documented_multi_word_env_keys_bind() {
    // `.split("_")` read `STORAGE_DIR` as `storage.dir`, a key that does
    // not exist, so every one of these overrides was dropped silently.
    let c = load_yaml(
        None,
        &[
            ("HOME_STILL_PERSONAL_STORAGE_DIR", "scans"),
            ("HOME_STILL_PERSONAL_COLLECTION_NAME", "my_docs"),
            ("HOME_STILL_PERSONAL_NAMING_MAX_INPUT_TOKENS", "512"),
            (
                "HOME_STILL_PERSONAL_NAMING_OLLAMA_URL",
                "http://llm.example:11434",
            ),
            ("HOME_STILL_PERSONAL_INGEST_INBOX", "drop"),
        ],
    )
    .unwrap();
    assert_eq!(c.storage_dir, "scans");
    assert_eq!(c.collection_name, "my_docs");
    assert_eq!(c.naming.max_input_tokens, 512);
    assert_eq!(c.naming.ollama_url, "http://llm.example:11434");
    assert_eq!(c.ingest_inbox, "drop");
    assert_eq!(c.naming.model, "qwen2.5:7b", "others keep their defaults");
}

#[test]
fn env_beats_the_file_and_the_file_beats_the_default() {
    let yaml = "personal:\n  storage_dir: from_file\n  naming:\n    model: file-model\n";
    let c = load_yaml(Some(yaml), &[]).unwrap();
    assert_eq!(
        (c.storage_dir.as_str(), c.naming.model.as_str()),
        ("from_file", "file-model")
    );
    let c = load_yaml(
        Some(yaml),
        &[("HOME_STILL_PERSONAL_STORAGE_DIR", "from_env")],
    )
    .unwrap();
    assert_eq!(c.storage_dir, "from_env");
    assert_eq!(c.naming.model, "file-model");
}

#[test]
fn an_env_key_that_names_nothing_is_an_error_not_silence() {
    let err = load_yaml(None, &[("HOME_STILL_PERSONAL_STORAGE", "x")])
        .unwrap_err()
        .to_string();
    assert!(err.contains("HOME_STILL_PERSONAL_STORAGE"), "{err}");
    // Variables for the other tools' sections are theirs, not ours.
    load_yaml(None, &[("HOME_STILL_PAPER_DOWNLOAD_PATH", "/x")]).unwrap();
}

#[test]
fn a_malformed_section_is_an_error_not_the_defaults() {
    for yaml in [
        "personal:\n  collection_name: [a]\n",
        "personal:\n  categories: not-a-list\n",
        "personal: [1, 2]\n",
        "home:\n  project_dir: [a]\n",
    ] {
        assert!(load_yaml(Some(yaml), &[]).is_err(), "{yaml}");
    }
    let err = load_yaml(Some("personal:\n  collection_name: [a]\n"), &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("personal"), "{err}");
}

#[test]
fn the_store_lives_under_home_project_dir() {
    let c = load_yaml(Some("home:\n  project_dir: /srv/hs\n"), &[]).unwrap();
    assert_eq!(c.root_dir(), PathBuf::from("/srv/hs/personal"));
    assert_eq!(c.markdown_dir(), PathBuf::from("/srv/hs/personal/markdown"));
    assert_eq!(c.inbox_dir(), PathBuf::from("/srv/hs/personal/inbox"));
}
