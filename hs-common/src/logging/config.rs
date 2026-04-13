use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_ROTATE_MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_ROTATE_INTERVAL_SECS: u64 = 60;
const DEFAULT_SHIP_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub enum StderrOutput {
    /// No stderr writes. Required for processes that use stdout/stderr as a
    /// protocol channel (e.g. `hs-mcp` stdio mode).
    Disabled,
    /// Emit human-readable lines to stderr at the given filter. RUST_LOG,
    /// when set, overrides this.
    EnvFilter(String),
    /// Shortcut for CLI binaries that expose `--verbose`/`--quiet` flags.
    /// Maps to: quiet → "error", verbose → "debug", otherwise → "warn".
    VerboseQuiet { verbose: bool, quiet: bool },
}

impl StderrOutput {
    pub(crate) fn filter_string(&self) -> Option<String> {
        match self {
            StderrOutput::Disabled => None,
            StderrOutput::EnvFilter(s) => Some(s.clone()),
            StderrOutput::VerboseQuiet { verbose, quiet } => Some(if *quiet {
                "error".into()
            } else if *verbose {
                "debug".into()
            } else {
                "warn".into()
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub service_name: String,
    pub hostname: String,
    pub spool_dir: PathBuf,
    pub rotate_max_bytes: u64,
    pub rotate_interval: Duration,
    pub file_filter: String,
    pub stderr: StderrOutput,
    pub ship_interval: Duration,
    pub s3_key_prefix: String,
    pub delete_on_ship_success: bool,
}

impl LoggingConfig {
    pub fn for_service(name: impl Into<String>) -> Self {
        let service_name = name.into();
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        let spool_dir = crate::resolve_log_dir().join("spool").join(&service_name);
        let s3_key_prefix = format!("{service_name}/{hostname}/");
        Self {
            service_name,
            hostname,
            spool_dir,
            rotate_max_bytes: DEFAULT_ROTATE_MAX_BYTES,
            rotate_interval: Duration::from_secs(DEFAULT_ROTATE_INTERVAL_SECS),
            file_filter: "info".into(),
            stderr: StderrOutput::EnvFilter("info".into()),
            ship_interval: Duration::from_secs(DEFAULT_SHIP_INTERVAL_SECS),
            s3_key_prefix,
            delete_on_ship_success: true,
        }
    }

    pub fn with_stderr(mut self, stderr: StderrOutput) -> Self {
        self.stderr = stderr;
        self
    }

    pub fn with_spool_dir(mut self, dir: PathBuf) -> Self {
        self.spool_dir = dir;
        self
    }

    pub fn with_file_filter(mut self, filter: impl Into<String>) -> Self {
        self.file_filter = filter.into();
        self
    }
}

/// Optional `logs:` section of `~/.home-still/config.yaml`. Callers who already
/// parse YAML via serde can deserialize this and call `apply_to` on a
/// `LoggingConfig` built from `LoggingConfig::for_service`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogsYaml {
    pub bucket: String,
    pub rotate_max_bytes: Option<u64>,
    pub rotate_interval_secs: Option<u64>,
    pub ship_interval_secs: Option<u64>,
}

impl Default for LogsYaml {
    fn default() -> Self {
        Self {
            bucket: "logs".into(),
            rotate_max_bytes: None,
            rotate_interval_secs: None,
            ship_interval_secs: None,
        }
    }
}

impl LogsYaml {
    pub fn apply_to(&self, cfg: &mut LoggingConfig) {
        if let Some(n) = self.rotate_max_bytes {
            cfg.rotate_max_bytes = n;
        }
        if let Some(s) = self.rotate_interval_secs {
            cfg.rotate_interval = Duration::from_secs(s);
        }
        if let Some(s) = self.ship_interval_secs {
            cfg.ship_interval = Duration::from_secs(s);
        }
    }
}
