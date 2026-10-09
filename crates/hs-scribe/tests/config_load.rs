//! Loading (RA-6, RA-97, N9): `ScribeConfig::from_file` / `AppConfig::from_file`
//! against a temporary home directory, with the process environment emptied
//! and then set per test (figment's `Jail`, which serialises the tests that
//! touch the environment).
//!
//! These live in their own test binary on purpose. `setenv` reallocates the
//! `environ` array, and on glibc before 2.41 a concurrent `getenv` from any
//! other thread (a `tempdir()`, `dirs::home_dir()`, pdfium's own lookups)
//! reads freed memory and can SIGSEGV. In the lib test binary hundreds of
//! unrelated tests run beside the ones that set variables; here every
//! environment read happens under the `Jail` lock, and the temporary homes
//! are rooted at a compile-time path so creating one reads no variable.

use hs_common::config_file::ConfigFile;
use hs_scribe::config::{AppConfig, BackendChoice, ConverterMode, ScribeConfig, SERVER_SECTION};
use std::path::PathBuf;

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
fn no_config_file_gives_documented_defaults_with_no_server_and_no_bus() {
    let (home, file) = home_with(None);
    let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
    assert!(cfg.servers.is_empty(), "no server is guessed");
    assert!(cfg.events.is_none(), "no bus is guessed");
    assert_eq!(cfg.output_dir, home.path().join("home-still/markdown"));
    assert_eq!(cfg.convert_timeout_secs, 900);
    let err = cfg.require_servers().unwrap_err().to_string();
    assert!(err.contains("scribe.servers"), "{err}");
    // The server side needs no file at all.
    with_env(&[], || AppConfig::from_file(&file)).unwrap();
}

#[test]
fn the_scribe_section_and_the_project_dir_are_honoured() {
    let (_home, file) = home_with(Some(
        "home:\n  project_dir: /srv/hs\nstorage:\n  backend: local\n  local:\n    root: /srv/hs\nscribe:\n  convert_timeout_secs: 1234\n  servers:\n    - http://host-a.example:7433\n    - url: http://host-b.example:7435\n      backend: olmocr\n      concurrency: 2\nevents:\n  backend: nats\n",
    ));
    let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
    assert_eq!(cfg.convert_timeout_secs, 1234);
    assert_eq!(cfg.output_dir, PathBuf::from("/srv/hs/markdown"));
    let urls: Vec<_> = cfg.servers.iter().map(|s| s.url.as_str()).collect();
    assert_eq!(
        urls,
        ["http://host-a.example:7433", "http://host-b.example:7435"]
    );
    assert_eq!(cfg.servers[1].backend, "olmocr");
    assert_eq!(cfg.servers[1].concurrency, 2);
    assert_eq!(
        cfg.events.as_ref().map(|e| e.backend.clone()),
        Some(hs_common::event_bus::EventsBackend::Nats)
    );
    assert_eq!(cfg.require_servers().unwrap().len(), 2);
}

#[test]
fn the_documented_env_override_reaches_the_client_config() {
    // `HS_SCRIBE_CONVERT_TIMEOUT_SECS` is documented on the field; the
    // loader used to merge the variable at the root and then read the
    // `scribe` key, so it never applied.
    let (_home, file) = home_with(Some("scribe:\n  convert_timeout_secs: 1234\n"));
    let cfg = with_env(&[("HS_SCRIBE_CONVERT_TIMEOUT_SECS", "77")], || {
        ScribeConfig::from_file(&file)
    })
    .unwrap();
    assert_eq!(cfg.convert_timeout_secs, 77, "env beats the file");
    let cfg = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
    assert_eq!(cfg.convert_timeout_secs, 1234);
    // A value that is not a number is an error, not the default.
    let err = with_env(&[("HS_SCRIBE_CONVERT_TIMEOUT_SECS", "soon")], || {
        ScribeConfig::from_file(&file)
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.to_ascii_lowercase().contains("convert_timeout_secs"),
        "{err}"
    );
}

#[test]
fn a_malformed_section_is_an_error_naming_it_never_the_defaults() {
    for (yaml, section) in [
        ("scribe:\n  convert_timeout_secs: soon\n", "scribe"),
        ("scribe:\n  servers: not-a-list\n", "scribe"),
        (
            "scribe:\n  timeout_policy:\n    floor_secs: 4000\n    ceiling_secs: 3600\n",
            "scribe",
        ),
        ("storage:\n  backend: carrier-pigeon\n", "storage"),
        ("events:\n  backend: carrier-pigeon\n", "events"),
        ("events:\n  nats: {url: nats://x:4222}\n", "events"),
        ("home:\n  project_dir: [a, b]\n", "home"),
    ] {
        let (_home, file) = home_with(Some(yaml));
        let err = with_env(&[], || ScribeConfig::from_file(&file))
            .expect_err(yaml)
            .to_string();
        assert!(err.contains(&format!("`{section}`")), "{yaml}: {err}");
    }
}

#[test]
fn the_server_config_reads_scribe_server_and_env_wins() {
    let (_home, file) = home_with(Some(
        "scribe:\n  vlm_concurrency: 99\nscribe_server:\n  vlm_concurrency: 9\n  backend: OpenAi\n  openai_url: http://llm.example:8080\n",
    ));
    let cfg = with_env(&[], || AppConfig::from_file(&file)).unwrap();
    assert_eq!(
        cfg.vlm_concurrency, 9,
        "the scribe: section is the client's"
    );
    assert_eq!(cfg.backend, BackendChoice::OpenAi);
    assert_eq!(cfg.openai_url, "http://llm.example:8080");

    let cfg = with_env(
        &[
            ("HS_SCRIBE_VLM_CONCURRENCY", "3"),
            ("HS_SCRIBE_CONVERTER", "olmocr"),
        ],
        || AppConfig::from_file(&file),
    )
    .unwrap();
    assert_eq!(cfg.vlm_concurrency, 3);
    assert_eq!(cfg.converter, ConverterMode::Olmocr);
    assert_eq!(
        cfg.backend,
        BackendChoice::OpenAi,
        "untouched keys keep the file's value"
    );
}

#[test]
fn a_bad_server_setting_stops_the_load_wherever_it_comes_from() {
    let (_home, ok) = home_with(Some("scribe_server:\n  vlm_concurrency: 9\n"));
    for (vars, what) in [
        (
            vec![("HS_SCRIBE_VLM_CONCURRENCY", "many")],
            "env value of the wrong type",
        ),
        (
            vec![("HS_SCRIBE_VLM_CONCURRENCY", "0")],
            "env value the server cannot run with",
        ),
        (
            vec![("HS_SCRIBE_BACKEND", "sglang")],
            "env backend that does not exist",
        ),
    ] {
        let err = with_env(&vars, || AppConfig::from_file(&ok)).expect_err(what);
        assert!(err.to_string().contains("scribe_server"), "{what}: {err}");
    }
    for yaml in [
        "scribe_server:\n  vlm_concurrency: 0\n",
        "scribe_server:\n  vlm_concurrency: lots\n",
        "scribe_server:\n  backend: sglang\n",
        "scribe_server: 7\n",
    ] {
        let (_home, file) = home_with(Some(yaml));
        let err = with_env(&[], || AppConfig::from_file(&file)).expect_err(yaml);
        assert!(err.to_string().contains("`scribe_server`"), "{yaml}: {err}");
    }
}

#[test]
fn a_stale_key_in_a_service_section_is_a_warning_not_a_failure() {
    let (_home, file) = home_with(Some(
        "scribe:\n  convert_timeout_secs: 77\n  stale_key: 1\nscribe_server:\n  vlm_concurency: 2\n",
    ));
    let client = with_env(&[], || ScribeConfig::from_file(&file)).unwrap();
    assert_eq!(client.convert_timeout_secs, 77, "known keys still apply");
    let server = with_env(&[], || AppConfig::from_file(&file)).unwrap();
    assert_eq!(
        server.vlm_concurrency,
        AppConfig::default().vlm_concurrency,
        "a misspelt key changes nothing (and is warned about)"
    );
    let tree = serde_json::to_value(AppConfig::default()).unwrap();
    let found = hs_common::config_file::unknown_keys(
        &file.section_json(SERVER_SECTION).unwrap().unwrap(),
        &tree,
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "vlm_concurency");
    assert!(found[0].valid.contains(&"vlm_concurrency".to_string()));
}
