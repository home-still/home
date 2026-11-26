//! Loading (RA-6): `DistillServerConfig::from_file` against a temporary home
//! directory, with the process environment emptied and then set per test
//! (figment's `Jail`, which serialises the tests that touch the environment).
//!
//! These live in their own test binary on purpose. `setenv` reallocates the
//! `environ` array, and on glibc before 2.41 a concurrent `getenv` from any
//! other thread (a `tempdir()`, `dirs::home_dir()`, TLS root loading) reads
//! freed memory and can SIGSEGV. In the lib test binary hundreds of unrelated
//! tests run beside the ones that set variables; here every environment read
//! happens under the `Jail` lock, and the temporary homes are rooted at a
//! compile-time path so creating one reads no variable.

use hs_common::config_file::ConfigFile;
use hs_distill::config::{DistillClientConfig, DistillServerConfig};

#[allow(clippy::result_large_err)] // figment's `Jail` closure type
fn with_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
    let mut out = None;
    figment::Jail::expect_with(|jail| {
        jail.clear_env();
        for (k, v) in vars {
            jail.set_env(k, v);
        }
        out = Some(f());
        Ok(())
    });
    out.expect("closure ran")
}

fn home_with(yaml: Option<&str>) -> (tempfile::TempDir, ConfigFile) {
    // `tempdir_in` with a compile-time root: `tempdir()` reads `TMPDIR`.
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    if let Some(yaml) = yaml {
        let path = home.path().join(hs_common::CONFIG_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, yaml).unwrap();
    }
    let file = ConfigFile::load_in(home.path()).unwrap();
    (home, file)
}

#[test]
fn yaml_collections_replace_the_default_list_and_host_port_are_read() {
    let (_home, file) = home_with(Some(
        "distill_server:\n  host: 127.0.0.1\n  port: 7444\n  collections: [only_this]\n",
    ));
    let loaded = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
    assert_eq!(loaded.collections, ["only_this"]);
    assert_eq!((loaded.host.as_str(), loaded.port), ("127.0.0.1", 7444));
    loaded.validate().unwrap();
    let served: Vec<_> = loaded.served_collections().collect();
    assert_eq!(served, ["academic_papers", "only_this"]);
}

#[test]
fn env_overrides_the_file() {
    let (_home, file) = home_with(Some(
        "distill_server:\n  port: 7444\n  myelin_url: http://127.0.0.1:7000/mcp\n",
    ));
    let loaded = with_env(
        &[
            ("HS_DISTILL_PORT", "7555"),
            ("HS_DISTILL_MYELIN_URL", "http://127.0.0.1:7001/mcp"),
        ],
        || DistillServerConfig::from_file(&file),
    )
    .unwrap();
    assert_eq!(loaded.port, 7555);
    assert_eq!(loaded.myelin_url, "http://127.0.0.1:7001/mcp");

    let from_file = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
    assert_eq!(from_file.port, 7444);
    assert_eq!(from_file.myelin_url, "http://127.0.0.1:7000/mcp");
}

#[test]
fn a_malformed_section_or_env_value_is_an_error_never_the_defaults() {
    for yaml in [
        "distill_server:\n  port: not-a-port\n",
        "distill_server:\n  collections: not-a-list\n",
        "distill_server: [1, 2]\n",
    ] {
        let (_home, file) = home_with(Some(yaml));
        let err = with_env(&[], || DistillServerConfig::from_file(&file))
            .expect_err(yaml)
            .to_string();
        assert!(err.contains("`distill_server`"), "{yaml}: {err}");
    }
    let (_home, ok) = home_with(None);
    let err = with_env(&[("HS_DISTILL_PORT", "http")], || {
        DistillServerConfig::from_file(&ok)
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("distill_server") && err.to_ascii_lowercase().contains("port"),
        "{err}"
    );
}

#[test]
fn removed_keys_still_load() {
    let yaml = "distill_server:\n  qdrant_url: http://localhost:6334\n  qdrant_data_dir: /x\n  embedding:\n    model: bge-m3\n    compute_device: cuda\n  hnsw:\n    m: 16\n  qdrant_upsert_batch: 10\n";
    let (_home, file) = home_with(Some(yaml));
    // The keys are inert; they are reported by name, never fatal.
    let loaded = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
    assert_eq!(loaded.myelin_url, DistillServerConfig::default().myelin_url);
}

#[test]
fn the_client_has_no_default_server_and_no_default_bus() {
    let (_home, file) = home_with(None);
    let cfg = with_env(&[], || DistillClientConfig::from_file(&file)).unwrap();
    assert!(cfg.servers.is_empty());
    assert!(cfg.events.is_none());
    let err = cfg.require_servers().unwrap_err().to_string();
    assert!(err.contains("distill.servers"), "{err}");

    let (_home, file) = home_with(Some(
        "distill:\n  servers: [http://host-a.example:7434]\n  index_timeout_secs: 600\nevents:\n  backend: noop\n",
    ));
    let cfg = with_env(&[], || DistillClientConfig::from_file(&file)).unwrap();
    assert_eq!(
        cfg.require_servers().unwrap(),
        ["http://host-a.example:7434"]
    );
    assert_eq!(cfg.index_timeout_secs, 600);
    assert!(cfg.events.is_some());
}

#[test]
fn a_malformed_client_section_is_an_error_naming_it() {
    for (yaml, section) in [
        ("distill:\n  servers: not-a-list\n", "distill"),
        ("distill:\n  index_timeout_secs: 0\n", "distill"),
        ("storage:\n  backend: carrier-pigeon\n", "storage"),
        ("events:\n  backend: carrier-pigeon\n", "events"),
        // A zero `ack_wait_secs` would give the heartbeat a zero interval.
        (
            "events:\n  backend: nats\n  nats:\n    ack_wait_secs: 0\n",
            "events",
        ),
    ] {
        let (_home, file) = home_with(Some(yaml));
        let err = with_env(&[], || DistillClientConfig::from_file(&file))
            .expect_err(yaml)
            .to_string();
        assert!(err.contains(&format!("`{section}`")), "{yaml}: {err}");
    }
}

#[test]
fn a_stale_or_misspelt_key_does_not_stop_either_config_from_loading() {
    let (_home, file) = home_with(Some(
        "distill_server:\n  port: 7555\n  myelin_urll: http://x\n  chunk_max_token: 4\ndistill:\n  index_timeout: 5\n",
    ));
    let server = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
    assert_eq!(server.port, 7555);
    assert_eq!(server.myelin_url, DistillServerConfig::default().myelin_url);
    with_env(&[], || DistillClientConfig::from_file(&file)).unwrap();
}
