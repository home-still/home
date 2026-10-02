//! Centralized object-storage logging for home-still services.
//!
//! Every binary writes JSONL to a local spool directory and a background
//! task uploads closed spool files to the configured `Storage` backend.
//! See the module's crate-level docs for the key layout.

pub mod config;
pub mod shipper;
pub mod spool;

pub use config::{InvalidLogsConfig, LoggingConfig, LogsYaml, SpoolCaps, StderrOutput};

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::storage::{Backend, LocalFsStorage, Storage, StorageConfig};
use spool::{Spool, SpoolWriter};

pub struct LoggingHandle {
    /// `None` when the spool dir could not be opened; the process then logs to
    /// whatever stderr channel is configured and ships nothing.
    spool: Option<Spool>,
    rotate_max_bytes: u64,
    rotate_interval: Duration,
    ship_interval: Duration,
    spool_caps: SpoolCaps,
    s3_key_prefix: String,
    delete_on_ship_success: bool,
    /// `home.log_dir`: where a local `storage:` keeps the log archive.
    log_dir: std::path::PathBuf,

    rotate_shutdown: watch::Sender<bool>,
    rotate_join: Option<JoinHandle<()>>,

    shipper_shutdown: Option<watch::Sender<bool>>,
    shipper_join: Option<JoinHandle<()>>,

    // Drop last so queued writes still flush after background tasks stop.
    _worker_guards: Vec<WorkerGuard>,
}

/// Install the global tracing subscriber and open the spool. Synchronous; may
/// be called before a tokio runtime exists. Background tasks (rotation +
/// shipping) are started by [`LoggingHandle::spawn_shipper`].
///
/// A spool directory that cannot be opened degrades to stderr-only logging
/// instead of failing: an unwritable log dir must never brick an invocation,
/// including the `hs upgrade` that would repair the config.
pub fn init(cfg: LoggingConfig) -> LoggingHandle {
    let stderr_filter = cfg.stderr.filter_string();

    let spool = match Spool::new(cfg.spool_dir.clone()) {
        Ok(spool) => Some(spool),
        Err(e) => {
            if stderr_filter.is_some() {
                // Printed straight to stderr: `--quiet` would swallow it and
                // the tracing subscriber is not installed yet.
                eprintln!(
                    "{}: log spool unavailable at {}: {e:#}; logging to stderr only",
                    cfg.service_name,
                    cfg.spool_dir.display()
                );
            }
            None
        }
    };

    let mut worker_guards = Vec::new();

    let file_layer = spool.as_ref().map(|spool| {
        let writer = SpoolWriter::new(spool.clone());
        let (non_blocking_file, file_worker_guard) = tracing_appender::non_blocking(writer);
        worker_guards.push(file_worker_guard);
        let file_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.file_filter));
        fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_writer(non_blocking_file)
            .with_filter(file_filter)
    });

    let stderr_layer = stderr_filter.map(|filter| {
        let (non_blocking_stderr, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());
        worker_guards.push(stderr_guard);
        fmt::layer()
            .with_target(false)
            .with_writer(non_blocking_stderr)
            .with_filter(EnvFilter::new(filter))
    });

    // `try_init` so a second call (e.g. in tests) stays a no-op. With neither
    // layer there is nothing to install and the global slot is left alone.
    if file_layer.is_some() || stderr_layer.is_some() {
        let _ = tracing_subscriber::registry()
            .with(file_layer)
            .with(stderr_layer)
            .try_init();
    }

    for notice in &cfg.notices {
        tracing::warn!("{notice}");
    }

    let (rotate_shutdown, _) = watch::channel(false);

    LoggingHandle {
        spool,
        rotate_max_bytes: cfg.rotate_max_bytes,
        rotate_interval: cfg.rotate_interval,
        ship_interval: cfg.ship_interval,
        spool_caps: cfg.spool_caps,
        s3_key_prefix: cfg.s3_key_prefix,
        delete_on_ship_success: cfg.delete_on_ship_success,
        log_dir: cfg.log_dir,
        rotate_shutdown,
        rotate_join: None,
        shipper_shutdown: None,
        shipper_join: None,
        _worker_guards: worker_guards,
    }
}

impl LoggingHandle {
    /// Run the spool janitor: rotation by size/age plus the byte/age caps on
    /// closed files. Needed whether or not anything ships the files, or an
    /// unshipped spool grows without bound. Idempotent.
    fn ensure_rotator(&mut self) {
        let Some(spool) = self.spool.clone() else {
            return;
        };
        if self.rotate_join.is_none() {
            self.rotate_join = Some(tokio::spawn(spool::run_rotate_controller(
                spool,
                self.rotate_max_bytes,
                self.rotate_interval,
                self.spool_caps,
                self.rotate_shutdown.subscribe(),
            )));
        }
    }

    /// Spawn the rotate controller + shipper onto the current tokio runtime.
    /// Idempotent per task — calling twice spawns only the tasks that aren't
    /// already running.
    pub fn spawn_shipper(&mut self, storage: Arc<dyn Storage>) -> anyhow::Result<()> {
        // No spool ⇒ nothing to rotate or ship.
        let Some(spool) = self.spool.clone() else {
            return Ok(());
        };
        self.ensure_rotator();
        if self.shipper_join.is_none() {
            let (tx, rx) = watch::channel(false);
            let join = tokio::spawn(shipper::run_shipper(
                spool.dir(),
                storage,
                self.s3_key_prefix.clone(),
                self.ship_interval,
                self.delete_on_ship_success,
                rx,
            ));
            self.shipper_shutdown = Some(tx);
            self.shipper_join = Some(join);
        }
        Ok(())
    }

    /// Start shipping spooled logs to the archive storage derived from the
    /// primary `storage:` config, or say once, in the log itself, why that
    /// is not happening. Logging must never fail a process, so a missing or
    /// broken storage config degrades to "spool on local disk, capped" —
    /// visibly, not silently. Call once after [`init`].
    pub async fn start_shipping(&mut self, primary: Option<&StorageConfig>, logs_bucket: &str) {
        if self.spool.is_none() {
            tracing::warn!("log shipping disabled: no spool directory (logging to stderr only)");
            return;
        }
        self.ensure_rotator();
        let Some(primary) = primary else {
            tracing::warn!(
                "log shipping disabled: no usable `storage:` section in the config file; \
                 closed log files stay in the local spool (capped)"
            );
            return;
        };
        let storage = match build_logs_storage(primary, logs_bucket, &self.log_dir).await {
            Ok(storage) => storage,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "log shipping disabled: cannot open the log archive storage; \
                     closed log files stay in the local spool (capped)"
                );
                return;
            }
        };
        if let Err(e) = self.spawn_shipper(storage) {
            tracing::warn!(error = %format!("{e:#}"), "log shipping disabled: shipper failed to start");
        }
    }

    /// Whether a spool dir was opened (`false` = degraded, stderr-only mode).
    #[cfg(test)]
    pub(crate) fn has_spool(&self) -> bool {
        self.spool.is_some()
    }

    /// Flush the current spool file and ask the shipper for a final pass.
    /// Call before the tokio runtime shuts down so pending logs reach storage.
    /// Safe to call even if `spawn_shipper` was never invoked.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(spool) = &self.spool {
            let _ = spool.rotate_now();
        }
        let _ = self.rotate_shutdown.send(true);
        if let Some(tx) = self.shipper_shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(join) = self.shipper_join.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), join).await;
        }
        if let Some(join) = self.rotate_join.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), join).await;
        }
        Ok(())
    }
}

impl Drop for LoggingHandle {
    fn drop(&mut self) {
        if let Some(spool) = &self.spool {
            let _ = spool.rotate_now();
        }
        let _ = self.rotate_shutdown.send(true);
        if let Some(tx) = &self.shipper_shutdown {
            let _ = tx.send(true);
        }
        if let Some(join) = self.shipper_join.take() {
            join.abort();
        }
        if let Some(join) = self.rotate_join.take() {
            join.abort();
        }
    }
}

/// Derive a `Storage` for the logs archive from the primary `StorageConfig`.
/// S3 reuses endpoint/credentials but swaps the bucket to `logs_bucket`.
/// Local writes to `{log_dir}/archive/` so log files don't land inside the
/// user's project directory.
pub async fn build_logs_storage(
    primary: &StorageConfig,
    logs_bucket: &str,
    log_dir: &std::path::Path,
) -> anyhow::Result<Arc<dyn Storage>> {
    let storage: Arc<dyn Storage> = match primary.backend {
        Backend::Local => Arc::new(LocalFsStorage::new(log_dir.join("archive"))),
        Backend::S3 => {
            let mut cfg = primary.clone();
            cfg.s3.bucket = logs_bucket.to_string();
            cfg.build()?
        }
    };
    storage.ensure_ready().await?;
    Ok(storage)
}

/// What logging setup needs from `~/.home-still/config.yaml`.
#[derive(Debug)]
pub struct ConfigSections {
    /// The `storage:` section, if the file has one. Without it closed log
    /// files stay in the local spool ([`LoggingHandle::start_shipping`] says
    /// so in the log).
    pub storage: Option<StorageConfig>,
    /// The `logs:` section, or its documented defaults when absent.
    pub logs: LogsYaml,
    /// `home.log_dir`, or `<home.project_dir>/logs`.
    pub log_dir: std::path::PathBuf,
    /// Warnings about the file found while reading these sections (unknown
    /// keys in `logs:`), to be logged once the logger exists: see
    /// [`LoggingConfig::notices`].
    pub notices: Vec<String>,
}

/// Read the `storage:` and `logs:` sections and the log directory from
/// `~/.home-still/config.yaml`.
///
/// An absent file or section yields the documented defaults. A file or
/// section that is present but malformed is an `Err` naming it: every
/// binary calls this before its logger exists, so the caller prints the
/// error and exits instead of starting on settings nobody wrote.
pub fn load_config_sections() -> Result<ConfigSections, crate::config_file::ConfigError> {
    sections_of(&crate::config_file::ConfigFile::load()?)
}

/// [`load_config_sections`] for an already-loaded file.
pub fn sections_of(
    file: &crate::config_file::ConfigFile,
) -> Result<ConfigSections, crate::config_file::ConfigError> {
    let logs_notices = match file.section_json("logs")? {
        Some(section) => {
            let known = serde_json::to_value(LogsYaml::default())
                .map_err(|e| crate::config_file::ConfigError::section(file.path(), "logs", e))?;
            crate::config_file::unknown_key_notices(file.path(), "logs", &section, &known, &[])
        }
        None => Vec::new(),
    };
    Ok(ConfigSections {
        storage: file.section("storage")?,
        logs: file.section("logs")?.unwrap_or_default(),
        log_dir: file.log_dir()?,
        notices: logs_notices,
    })
}

impl ConfigSections {
    /// The sections for a command that must run when the config file is
    /// missing or broken (`hs config init` / `hs config path` exist to
    /// create and locate it): no `storage:`, default `logs:`, the default
    /// log directory. Only those commands use this; everything else calls
    /// [`load_config_sections`] and refuses to run on a malformed file.
    pub fn without_config_file() -> Self {
        Self {
            storage: None,
            logs: LogsYaml::default(),
            log_dir: crate::default_project_dir().join("logs"),
            notices: Vec::new(),
        }
    }

    /// The `LoggingConfig` for `service` with the `logs:` overrides applied.
    pub fn logging_config(
        &self,
        service: &str,
        stderr: StderrOutput,
    ) -> Result<LoggingConfig, InvalidLogsConfig> {
        let mut cfg = LoggingConfig::for_service(service, &self.log_dir).with_stderr(stderr);
        self.logs.apply_to(&mut cfg)?;
        cfg.notices = self.notices.clone();
        Ok(cfg)
    }
}

/// Report a configuration error found while setting up logging and exit with
/// status 2. Every binary calls this before its logger exists, so the only
/// channel left is stderr (the journal, for a supervised service).
pub fn exit_on_config_error(service: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("{service}: {error}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spool dir that cannot be opened must degrade, not abort: `hs` used to
    /// panic here, which bricked `hs upgrade` on hosts whose log dir was
    /// unwritable.
    #[tokio::test]
    async fn spool_failure_degrades_to_stderr_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A *file* where the spool dir should be: `create_dir_all` fails with
        // ENOTDIR, which is also what an unwritable mount gives us.
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"").expect("write blocker file");

        let cfg = LoggingConfig::for_service("hs-logging-test", tmp.path())
            .with_spool_dir(blocked.join("spool"));
        let handle = init(cfg);

        assert!(!handle.has_spool());
        handle.shutdown().await.expect("shutdown is infallible");
    }

    #[tokio::test]
    async fn writable_spool_dir_is_opened() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool_dir = tmp.path().join("spool");
        let cfg = LoggingConfig::for_service("hs-logging-test", tmp.path())
            .with_spool_dir(spool_dir.clone())
            .with_stderr(StderrOutput::Disabled);
        let handle = init(cfg);
        assert!(handle.has_spool());
        assert!(spool_dir.join(spool::CURRENT_FILE).exists());
        handle.shutdown().await.expect("shutdown is infallible");
    }

    /// `MakeWriter` that records everything the subscriber prints.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let mut buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
            buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Captured {
        fn text(&self) -> String {
            let buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&buf).into_owned()
        }
    }

    fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
        let cap = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        (cap, tracing::subscriber::set_default(sub))
    }

    fn quiet_handle(tmp: &std::path::Path) -> LoggingHandle {
        init(
            LoggingConfig::for_service("hs-logging-test", tmp)
                .with_spool_dir(tmp.join("spool"))
                .with_stderr(StderrOutput::Disabled),
        )
    }

    /// RA-80: no usable `storage:` section used to disable shipping with no
    /// word to anyone; it now says so once, and the spool is still bounded.
    #[tokio::test]
    async fn missing_storage_config_logs_once_and_still_runs_the_janitor() {
        let (cap, _guard) = capture();
        let tmp = tempfile::tempdir().unwrap();
        let mut handle = quiet_handle(tmp.path());

        handle.start_shipping(None, "logs").await;

        let text = cap.text();
        assert_eq!(text.matches("log shipping disabled").count(), 1, "{text}");
        assert!(text.contains("no usable `storage:`"), "{text}");
        assert!(handle.shipper_join.is_none(), "nothing should ship");
        assert!(handle.rotate_join.is_some(), "spool must still be capped");
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unbuildable_archive_storage_logs_the_cause() {
        let (cap, _guard) = capture();
        let tmp = tempfile::tempdir().unwrap();
        let mut handle = quiet_handle(tmp.path());
        // S3 with no endpoint/credentials: rejected before any network I/O.
        let primary = StorageConfig {
            backend: Backend::S3,
            ..StorageConfig::default()
        };

        handle.start_shipping(Some(&primary), "logs").await;

        let text = cap.text();
        assert_eq!(text.matches("log shipping disabled").count(), 1, "{text}");
        assert!(
            text.contains("cannot open the log archive storage"),
            "{text}"
        );
        assert!(handle.shipper_join.is_none());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn without_a_spool_dir_the_degraded_mode_is_announced() {
        let (cap, _guard) = capture();
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        let mut handle = init(
            LoggingConfig::for_service("hs-logging-test", tmp.path())
                .with_spool_dir(blocked.join("spool"))
                .with_stderr(StderrOutput::Disabled),
        );
        handle.start_shipping(None, "logs").await;
        assert!(cap.text().contains("no spool directory"), "{}", cap.text());
        assert!(handle.rotate_join.is_none());
        handle.shutdown().await.unwrap();
    }

    fn home_with(config: Option<&str>) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if let Some(text) = config {
            let path = home.path().join(crate::CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        home
    }

    fn sections(
        home: &tempfile::TempDir,
    ) -> Result<ConfigSections, crate::config_file::ConfigError> {
        sections_of(&crate::config_file::ConfigFile::load_in(home.path())?)
    }

    #[test]
    fn an_absent_file_or_section_gives_the_documented_defaults() {
        for config in [None, Some("scribe: {}\n")] {
            let home = home_with(config);
            let s = sections(&home).unwrap();
            assert!(s.storage.is_none());
            assert_eq!(s.logs.bucket, "logs");
            assert_eq!(s.log_dir, home.path().join("home-still").join("logs"));
        }
    }

    #[test]
    fn a_malformed_storage_or_logs_section_is_an_error_naming_it() {
        // Each of these used to be swallowed and replaced by "no storage,
        // default logs", so a typo silently turned log shipping off.
        for (yaml, section) in [
            ("storage:\n  backend: carrier-pigeon\n", "storage"),
            ("storage: [1, 2]\n", "storage"),
            ("logs:\n  ship_interval_secs: soon\n", "logs"),
            ("logs: 7\n", "logs"),
        ] {
            let home = home_with(Some(yaml));
            let err = sections(&home).unwrap_err().to_string();
            assert!(err.contains(&format!("`{section}`")), "{yaml}: {err}");
            assert!(err.contains("config.yaml"), "{yaml}: {err}");
        }
        let home = home_with(Some("storage: {backend: local\n"));
        assert!(sections(&home).is_err(), "unparsable YAML");
    }

    #[test]
    fn log_dir_follows_the_home_section() {
        let home = home_with(Some(
            "home:\n  project_dir: /srv/hs\nstorage:\n  backend: local\nlogs:\n  bucket: audit\n",
        ));
        let s = sections(&home).unwrap();
        assert_eq!(s.log_dir, std::path::PathBuf::from("/srv/hs/logs"));
        assert_eq!(s.logs.bucket, "audit");
        assert_eq!(s.storage.unwrap().backend, Backend::Local);
    }

    #[test]
    fn unknown_logs_keys_are_warnings_carried_to_the_logger_not_errors() {
        let home = home_with(Some(
            "logs:\n  bucket: audit\n  ship_interval: 30\n  rotate_max_bytez: 1\n",
        ));
        let s = sections(&home).expect("a stale key must not stop the process");
        assert_eq!(s.logs.bucket, "audit");
        assert_eq!(s.notices.len(), 2, "{:?}", s.notices);
        assert!(
            s.notices[0].contains("logs.rotate_max_bytez"),
            "{:?}",
            s.notices
        );
        assert!(
            s.notices[1].contains("logs.ship_interval"),
            "{:?}",
            s.notices
        );
        assert!(
            s.notices[0].contains("ship_interval_secs"),
            "names the valid keys"
        );
        // They travel with the logging config, which `init` logs from once
        // the subscriber exists (a warn! made while reading is dropped).
        let cfg = s.logging_config("t", StderrOutput::Disabled).unwrap();
        assert_eq!(cfg.notices, s.notices);

        let clean = sections(&home_with(Some("logs:\n  bucket: audit\n"))).unwrap();
        assert!(clean.notices.is_empty());
    }
}
