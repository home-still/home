use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const DEFAULT_ROTATE_MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_ROTATE_INTERVAL_SECS: u64 = 60;
const DEFAULT_SHIP_INTERVAL_SECS: u64 = 30;
const DEFAULT_SPOOL_MAX_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_SPOOL_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

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

/// Upper bounds on closed spool files waiting to be shipped. When storage is
/// down, or shipping is disabled, they are all that stands between a log
/// line and an ever-growing spool directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolCaps {
    /// Total bytes of closed spool files kept; the oldest are dropped first.
    pub max_bytes: u64,
    /// Closed spool files older than this are dropped.
    pub max_age: Duration,
}

impl Default for SpoolCaps {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_SPOOL_MAX_BYTES,
            max_age: Duration::from_secs(DEFAULT_SPOOL_MAX_AGE_SECS),
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
    pub spool_caps: SpoolCaps,
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
            spool_caps: SpoolCaps::default(),
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
    pub spool_max_bytes: Option<u64>,
    pub spool_max_age_secs: Option<u64>,
}

impl Default for LogsYaml {
    fn default() -> Self {
        Self {
            bucket: "logs".into(),
            rotate_max_bytes: None,
            rotate_interval_secs: None,
            ship_interval_secs: None,
            spool_max_bytes: None,
            spool_max_age_secs: None,
        }
    }
}

/// A `logs:` value the logging stack cannot run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLogsConfig {
    pub key: &'static str,
    pub reason: &'static str,
}

impl std::fmt::Display for InvalidLogsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid `logs.{}` in config: {}", self.key, self.reason)
    }
}

impl std::error::Error for InvalidLogsConfig {}

impl LogsYaml {
    /// Apply the overrides to `cfg`. A value the shipper cannot run with is
    /// an `Err` and `cfg` is left untouched: `ship_interval_secs: 0` used to
    /// reach `tokio::time::interval(ZERO)` and panic (an abort in release).
    pub fn apply_to(&self, cfg: &mut LoggingConfig) -> Result<(), InvalidLogsConfig> {
        let positive = |key: &'static str, v: Option<u64>| match v {
            Some(0) => Err(InvalidLogsConfig {
                key,
                reason: "must be at least 1",
            }),
            _ => Ok(()),
        };
        positive("ship_interval_secs", self.ship_interval_secs)?;
        positive("rotate_interval_secs", self.rotate_interval_secs)?;
        positive("rotate_max_bytes", self.rotate_max_bytes)?;
        positive("spool_max_bytes", self.spool_max_bytes)?;
        positive("spool_max_age_secs", self.spool_max_age_secs)?;

        if let Some(n) = self.rotate_max_bytes {
            cfg.rotate_max_bytes = n;
        }
        if let Some(s) = self.rotate_interval_secs {
            cfg.rotate_interval = Duration::from_secs(s);
        }
        if let Some(s) = self.ship_interval_secs {
            cfg.ship_interval = Duration::from_secs(s);
        }
        if let Some(n) = self.spool_max_bytes {
            cfg.spool_caps.max_bytes = n;
        }
        if let Some(s) = self.spool_max_age_secs {
            cfg.spool_caps.max_age = Duration::from_secs(s);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_ship_interval_is_rejected_and_leaves_the_config_untouched() {
        let mut cfg = LoggingConfig::for_service("t");
        let before = cfg.ship_interval;
        let yaml = LogsYaml {
            ship_interval_secs: Some(0),
            rotate_max_bytes: Some(1234),
            ..Default::default()
        };
        let err = yaml.apply_to(&mut cfg).unwrap_err();
        assert_eq!(err.key, "ship_interval_secs");
        assert_eq!(cfg.ship_interval, before);
        assert_ne!(cfg.rotate_max_bytes, 1234, "partial apply on error");
    }

    #[test]
    fn every_zero_valued_knob_is_rejected() {
        for (key, yaml) in [
            (
                "rotate_interval_secs",
                LogsYaml {
                    rotate_interval_secs: Some(0),
                    ..Default::default()
                },
            ),
            (
                "rotate_max_bytes",
                LogsYaml {
                    rotate_max_bytes: Some(0),
                    ..Default::default()
                },
            ),
            (
                "spool_max_bytes",
                LogsYaml {
                    spool_max_bytes: Some(0),
                    ..Default::default()
                },
            ),
            (
                "spool_max_age_secs",
                LogsYaml {
                    spool_max_age_secs: Some(0),
                    ..Default::default()
                },
            ),
        ] {
            let mut cfg = LoggingConfig::for_service("t");
            assert_eq!(yaml.apply_to(&mut cfg).unwrap_err().key, key);
        }
    }

    #[test]
    fn valid_overrides_apply_and_defaults_are_bounded() {
        let mut cfg = LoggingConfig::for_service("t");
        assert!(cfg.spool_caps.max_bytes > 0 && !cfg.spool_caps.max_age.is_zero());
        let yaml = LogsYaml {
            ship_interval_secs: Some(5),
            spool_max_bytes: Some(1 << 20),
            spool_max_age_secs: Some(3600),
            ..Default::default()
        };
        yaml.apply_to(&mut cfg).unwrap();
        assert_eq!(cfg.ship_interval, Duration::from_secs(5));
        assert_eq!(cfg.spool_caps.max_bytes, 1 << 20);
        assert_eq!(cfg.spool_caps.max_age, Duration::from_secs(3600));
    }
}
