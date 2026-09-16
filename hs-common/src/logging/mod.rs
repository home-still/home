//! Centralized object-storage logging for home-still services.
//!
//! Every binary writes JSONL to a local spool directory and a background
//! task uploads closed spool files to the configured `Storage` backend.
//! See the module's crate-level docs for the key layout.

pub mod config;
pub mod shipper;
pub mod spool;

pub use config::{LoggingConfig, LogsYaml, StderrOutput};

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
    s3_key_prefix: String,
    delete_on_ship_success: bool,

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

    let (rotate_shutdown, _) = watch::channel(false);

    LoggingHandle {
        spool,
        rotate_max_bytes: cfg.rotate_max_bytes,
        rotate_interval: cfg.rotate_interval,
        ship_interval: cfg.ship_interval,
        s3_key_prefix: cfg.s3_key_prefix,
        delete_on_ship_success: cfg.delete_on_ship_success,
        rotate_shutdown,
        rotate_join: None,
        shipper_shutdown: None,
        shipper_join: None,
        _worker_guards: worker_guards,
    }
}

impl LoggingHandle {
    /// Spawn the rotate controller + shipper onto the current tokio runtime.
    /// Idempotent per task — calling twice spawns only the tasks that aren't
    /// already running.
    pub fn spawn_shipper(&mut self, storage: Arc<dyn Storage>) -> anyhow::Result<()> {
        // No spool ⇒ nothing to rotate or ship.
        let Some(spool) = self.spool.clone() else {
            return Ok(());
        };
        if self.rotate_join.is_none() {
            self.rotate_join = Some(tokio::spawn(spool::run_rotate_controller(
                spool.clone(),
                self.rotate_max_bytes,
                self.rotate_interval,
                self.rotate_shutdown.subscribe(),
            )));
        }
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
) -> anyhow::Result<Arc<dyn Storage>> {
    let storage: Arc<dyn Storage> = match primary.backend {
        Backend::Local => {
            let archive_root = crate::resolve_log_dir().join("archive");
            Arc::new(LocalFsStorage::new(archive_root))
        }
        Backend::S3 => {
            let mut cfg = primary.clone();
            cfg.s3.bucket = logs_bucket.to_string();
            cfg.build()?
        }
    };
    storage.ensure_ready().await?;
    Ok(storage)
}

/// Read the `storage:` and `logs:` sections from `~/.home-still/config.yaml`.
/// Returns defaults on any error — this is only used for logging setup, so we
/// never want parsing failures to crash a binary. Callers can combine the
/// result with [`build_logs_storage`] and [`LogsYaml::apply_to`].
pub fn load_config_sections() -> (Option<StorageConfig>, LogsYaml) {
    let config_path = match dirs::home_dir() {
        Some(h) => h.join(crate::CONFIG_REL_PATH),
        None => return (None, LogsYaml::default()),
    };
    let contents = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return (None, LogsYaml::default()),
    };
    let doc: serde_yaml_ng::Value = match serde_yaml_ng::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return (None, LogsYaml::default()),
    };
    let storage = doc
        .get("storage")
        .and_then(|v| serde_yaml_ng::from_value::<StorageConfig>(v.clone()).ok());
    let logs = doc
        .get("logs")
        .and_then(|v| serde_yaml_ng::from_value::<LogsYaml>(v.clone()).ok())
        .unwrap_or_default();
    (storage, logs)
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

        let cfg =
            LoggingConfig::for_service("hs-logging-test").with_spool_dir(blocked.join("spool"));
        let handle = init(cfg);

        assert!(!handle.has_spool());
        handle.shutdown().await.expect("shutdown is infallible");
    }

    #[tokio::test]
    async fn writable_spool_dir_is_opened() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool_dir = tmp.path().join("spool");
        let cfg = LoggingConfig::for_service("hs-logging-test")
            .with_spool_dir(spool_dir.clone())
            .with_stderr(StderrOutput::Disabled);
        let handle = init(cfg);
        assert!(handle.has_spool());
        assert!(spool_dir.join(spool::CURRENT_FILE).exists());
        handle.shutdown().await.expect("shutdown is infallible");
    }
}
