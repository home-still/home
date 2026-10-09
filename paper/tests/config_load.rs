//! `Config::load` / `Config::load_from` under a controlled process
//! environment (figment's `Jail`, which serialises the tests that touch it).
//!
//! These live in their own test binary on purpose. `setenv` reallocates the
//! `environ` array, and on glibc before 2.41 a concurrent `getenv` from any
//! other thread (a `tempdir()`, `dirs::home_dir()`, TLS root loading) reads
//! freed memory and can SIGSEGV. In the lib test binary hundreds of unrelated
//! tests run beside the ones that set variables; here every test holds the
//! `Jail` lock.

use hs_common::config_file::ConfigFile;
use hs_common::CONFIG_REL_PATH;
use paper::config::Config;
use std::path::{Path, PathBuf};

/// Run `f` with a hermetic env: no inherited vars, `$HOME` an empty dir.
#[allow(clippy::result_large_err)] // figment's `Jail` closure type
fn with_env(vars: &[(&str, &str)], f: impl FnOnce(anyhow::Result<Config>)) {
    figment::Jail::expect_with(|jail| {
        jail.clear_env();
        jail.set_env("HOME", jail.directory().display().to_string());
        for (k, v) in vars {
            jail.set_env(k, v);
        }
        f(Config::load());
        Ok(())
    });
}

#[test]
fn documented_multi_word_env_keys_bind() {
    with_env(
        &[
            // README: `HOME_STILL_PAPER_DOWNLOAD_PATH=/tmp/papers`
            ("HOME_STILL_PAPER_DOWNLOAD_PATH", "/tmp/papers"),
            ("HOME_STILL_PAPER_DOWNLOAD_TIMEOUT_SECS", "77"),
            (
                "HOME_STILL_PAPER_PROVIDERS_SEMANTIC_SCHOLAR_RATE_LIMIT_INTERVAL_MS",
                "2500",
            ),
            ("HOME_STILL_PAPER_RESILIENCE_CB_FAILURE_THRESHOLD", "9"),
            (
                "HOME_STILL_PAPER_PROVIDERS_CROSSREF_MAILTO",
                "ops@example.org",
            ),
            ("HOME_STILL_STORAGE_S3_ACCESS_KEY", "akey"),
            ("HOME_STILL_STORAGE_BACKEND", "s3"),
        ],
        |loaded| {
            let c = loaded.unwrap();
            assert_eq!(c.download_path, PathBuf::from("/tmp/papers"));
            assert_eq!(c.download.timeout_secs, 77);
            assert_eq!(c.providers.semantic_scholar.rate_limit_interval_ms, 2500);
            assert_eq!(c.resilience.cb_failure_threshold, 9);
            assert_eq!(
                c.resilience.cb_initial_backoff_secs, 10,
                "others keep defaults"
            );
            assert_eq!(
                c.providers.crossref.mailto.as_deref(),
                Some("ops@example.org")
            );
            assert_eq!(c.storage.s3.access_key, "akey");
            assert_eq!(c.storage.backend, hs_common::storage::config::Backend::S3);
        },
    );
}

#[test]
fn an_env_key_that_names_nothing_is_an_error_not_silence() {
    with_env(&[("HOME_STILL_PAPER_DOWNLOAD_TIMEOUT", "5")], |loaded| {
        let err = loaded.unwrap_err().to_string();
        assert!(err.contains("HOME_STILL_PAPER_DOWNLOAD_TIMEOUT"), "{err}");
    });
}

#[test]
fn an_env_value_that_fails_validation_fails_the_load() {
    with_env(
        &[(
            "HOME_STILL_PAPER_PROVIDERS_ARXIV_RATE_LIMIT_INTERVAL_MS",
            "0",
        )],
        |loaded| assert!(loaded.is_err()),
    );
}

// ── Config file sections (RA-6, RA-7) ──────────────────────────────────

/// Load against a config file with `yaml` as its content (or none) under
/// a hermetic environment.
#[allow(clippy::result_large_err)] // figment's `Jail` closure type
fn load_yaml(yaml: Option<&str>) -> anyhow::Result<Config> {
    let mut out = None;
    figment::Jail::expect_with(|jail| {
        jail.clear_env();
        let home = jail.directory().to_path_buf();
        if let Some(yaml) = yaml {
            let path = home.join(CONFIG_REL_PATH);
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
fn a_malformed_storage_or_events_section_is_an_error_not_the_defaults() {
    // Each used to be replaced by `unwrap_or_default()`: downloads went
    // to the local default and events to a bus that drops everything.
    for (yaml, section) in [
        ("storage:\n  backend: carrier-pigeon\n", "storage"),
        ("storage: [1, 2]\n", "storage"),
        ("events:\n  backend: carrier-pigeon\n", "events"),
        ("events:\n  nats:\n    url: nats://x:4222\n", "events"),
        (
            "events:\n  backend: nats\n  nats:\n    user: only-a-user\n",
            "events",
        ),
        ("paper:\n  download:\n    timeout_secs: soon\n", "paper"),
    ] {
        let err = format!("{:#}", load_yaml(Some(yaml)).expect_err(yaml));
        assert!(err.contains(section), "{yaml}: {err}");
    }
}

#[test]
fn an_absent_events_section_is_no_bus_and_refuses_to_build_one() {
    let config = load_yaml(Some("storage:\n  backend: local\n")).unwrap();
    assert!(config.events.is_none());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let err = rt
        .block_on(config.build_event_bus())
        .err()
        .expect("no bus may be invented")
        .to_string();
    assert!(err.contains("events.backend"), "{err}");

    let config = load_yaml(Some("events:\n  backend: noop\n")).unwrap();
    assert!(rt.block_on(config.build_event_bus()).is_ok());
}

#[test]
fn the_default_download_path_follows_home_project_dir_and_an_explicit_one_wins() {
    // `/srv/hs` has no drive on Windows, where a relative project dir is
    // refused.
    let project = if cfg!(windows) {
        "C:/srv/hs"
    } else {
        "/srv/hs"
    };
    let storage = format!("storage:\n  backend: local\n  local:\n    root: {project}\n");
    let config = load_yaml(Some(&format!("home:\n  project_dir: {project}\n{storage}"))).unwrap();
    assert_eq!(config.download_path, PathBuf::from(project).join("papers"));
    let config = load_yaml(Some(&format!(
        "home:\n  project_dir: {project}\n{storage}paper:\n  download_path: /elsewhere/papers\n",
    )))
    .unwrap();
    assert_eq!(config.download_path, PathBuf::from("/elsewhere/papers"));
    // A broken `home` section is not "the default project dir".
    assert!(load_yaml(Some("home:\n  project_dir: [a]\n")).is_err());
}

#[test]
fn an_unknown_key_where_data_lives_is_an_error_and_elsewhere_a_warning() {
    // `storage.root` for `storage.local.root` ran on the default root.
    for yaml in [
        "storage:\n  backend: local\n  root: /x\n",
        "home:\n  project_directory: /x\n",
    ] {
        let err = format!("{:#}", load_yaml(Some(yaml)).expect_err(yaml));
        assert!(
            err.contains("unknown field") || err.contains("unknown"),
            "{yaml}: {err}"
        );
    }
    // A stale key in `paper:` is ignored (and warned about), not fatal.
    let config = load_yaml(Some(
        "paper:\n  download:\n    core_api_key: stale\n    timeout_secs: 55\n",
    ))
    .expect("stale keys in service sections must not stop the load");
    assert_eq!(config.download.timeout_secs, 55);
}
