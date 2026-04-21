use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::storage::{Storage, StorageConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Resolve project_dir from ~/.home-still/config.yaml or default to ~/home-still.
fn resolve_project_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(".home-still/config.yaml");
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        let mut in_home = false;
        for line in contents.lines() {
            let t = line.trim();
            if t.starts_with('#') || t.is_empty() {
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_home = t.starts_with("home:");
            }
            if in_home {
                if let Some(val) = t.strip_prefix("project_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        if let Some(rest) = val.strip_prefix("~/") {
                            return home.join(rest);
                        }
                        return PathBuf::from(val);
                    }
                }
            }
        }
    }
    home.join("home-still")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum BackendChoice {
    #[default]
    Ollama,
    Cloud,
    OpenAi,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum PipelineMode {
    FullPage,
    #[default]
    PerRegion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub ollama_url: String,
    pub model: String,
    pub cloud_api_key: Option<String>,
    pub cloud_url: String,
    pub openai_url: String,
    pub backend: BackendChoice,
    /// Wall-clock deadline (seconds) for a single PDF convert on the server.
    /// The handler wraps `process_pdf_*` in `tokio::time::timeout()` — when
    /// this fires, the inner future chain is dropped, which cancels every
    /// in-flight VLM request to Ollama (reqwest is cancel-safe) and releases
    /// the VLM semaphore permit. Without this, a slow Ollama backend (e.g.
    /// Apple Silicon on big_mac / mac_air) could leave the handler polling
    /// forever after the client disconnected, wedging all convert slots until
    /// the process was restarted. Matches the client-side
    /// `ScribeConfig::convert_timeout_secs` default (900s); tune via
    /// `HS_SCRIBE_CONVERT_DEADLINE_SECS`.
    pub convert_deadline_secs: u64,
    pub dpi: u16,
    pub parallel: usize,
    pub pipeline_mode: PipelineMode,
    pub layout_model_path: String,
    pub table_model_path: String,
    pub region_parallel: usize,
    pub use_cuda: bool,
    pub max_image_dim: u32,
    pub vlm_concurrency: usize,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ollama_url: "http://localhost:11434".into(),
            model: "glm-ocr:latest".into(),
            cloud_api_key: None,
            cloud_url: "https://api.z.ai/api/paas/v4/layout_parsing".into(),
            openai_url: "http://localhost:8080".into(),
            backend: BackendChoice::Ollama,
            convert_deadline_secs: 900,
            dpi: 200,
            parallel: 1,
            pipeline_mode: PipelineMode::PerRegion,
            layout_model_path: "pp-doclayoutv3.onnx".into(),
            table_model_path: "slanet-plus.onnx".into(),
            region_parallel: 4,
            use_cuda: true,
            max_image_dim: 1800,
            vlm_concurrency: 4,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let config_path = dirs::config_dir()
            .map(|d| d.join("home-still").join("config.yaml"))
            .unwrap_or_default();

        Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Yaml::file(config_path).nested())
            .merge(Env::prefixed("HS_SCRIBE_"))
            .extract()
            .map_err(Box::new)
    }

    /// Resolve a model filename to an absolute path.
    /// If already absolute and exists, use as-is.
    /// Otherwise look in `~/.home-still/models/`.
    pub fn resolve_model_path(name: &str) -> PathBuf {
        let p = PathBuf::from(name);
        if p.is_absolute() && p.exists() {
            return p;
        }
        if p.exists() {
            return p;
        }
        let models_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".home-still")
            .join("models");
        models_dir.join(name)
    }

    pub fn resolved_layout_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.layout_model_path)
    }

    pub fn resolved_table_model_path(&self) -> PathBuf {
        Self::resolve_model_path(&self.table_model_path)
    }
}

/// Client-side scribe configuration (server list, directories).
/// Loaded from ~/.home-still/config.yaml under the "scribe" section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScribeConfig {
    pub output_dir: PathBuf,
    pub watch_dir: PathBuf,
    pub corrupted_dir: PathBuf,
    pub catalog_dir: PathBuf,
    pub servers: Vec<String>,
    /// When false, skip local scribe server init/start (client-only mode).
    /// Machines that only run the watcher and forward to remote scribe servers
    /// should set this to false.
    pub local_server: bool,
    /// Polling interval for the client-side inbox watcher
    /// (`hs scribe inbox run`). The inbox daemon sweeps
    /// `papers/manually_downloaded/` on the configured Storage at this
    /// cadence, relocates eligible files to `papers/<shard>/...`, and
    /// publishes `papers.ingested` on NATS. Default 30 seconds; a short
    /// value drains drops quickly, a long value is gentler on S3 list
    /// cost. Must be ≥ 1.
    #[serde(default = "default_inbox_poll_interval_secs")]
    pub inbox_poll_interval_secs: u64,
    /// Request timeout (seconds) for `ScribeClient::convert` /
    /// `convert_with_progress`. Caps a single PDF conversion so a
    /// stuck backend (e.g. Ollama hang) can't freeze the subscriber.
    /// Default 900s (15min) fits long multi-page VLM runs on Metal
    /// with headroom; raise for outlier workloads via the
    /// `HS_SCRIBE_CONVERT_TIMEOUT_SECS` env override.
    #[serde(default = "default_convert_timeout_secs")]
    pub convert_timeout_secs: u64,
    /// Ollama `OLLAMA_NUM_PARALLEL` auto-tuner knobs. Consumed by
    /// `hs scribe autotune`, which hill-climbs against observed
    /// per-host scribe throughput.
    #[serde(default)]
    pub autotune: AutotuneConfig,
    /// Storage backend (loaded from top-level `storage:` section, not `scribe.storage`).
    #[serde(skip)]
    pub storage: StorageConfig,
    /// Event bus (loaded from top-level `events:` section).
    #[serde(skip)]
    pub events: EventBusConfig,
}

/// Per-host knobs for `hs scribe autotune`. All fields have sane
/// defaults; the autotuner works out of the box.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutotuneConfig {
    /// URL of the scribe-server on the same host as Ollama.
    pub scribe_url: String,
    /// How long between ticks. Each tick restarts Ollama once, so this
    /// is also the "per-host disruption budget" — default 30 min.
    pub tick_interval_secs: u64,
    /// Wait after each Ollama restart before starting the measurement
    /// window. Gives the model time to warm up and in-flight converts
    /// to drain.
    pub warmup_secs: u64,
    /// Measurement window: count scribe's `total_conversions` delta
    /// across this interval. Shorter → noisier; longer → slower to
    /// converge. Default 24 min (so warmup + measure fits in a 30 min
    /// tick with headroom).
    pub measure_secs: u64,
    /// Candidate values the hill-climber walks. Must be strictly
    /// increasing and have at least 2 entries.
    pub values: Vec<u32>,
    /// Ratio that counts as a real improvement, e.g. 1.05 = needs +5%.
    pub improvement_threshold: f64,
    /// Ratio below which we call it a regression and step back, e.g.
    /// 0.90 = backs off at a -10% drop.
    pub regression_threshold: f64,
    /// Number of inconclusive ticks (rate within the two thresholds)
    /// before the tuner marks itself converged and stops stepping.
    pub converge_after_stable: u32,
    /// Where the tuner persists its rolling history + current state.
    /// Survives across restarts.
    pub state_path: PathBuf,
}

impl Default for AutotuneConfig {
    fn default() -> Self {
        let state_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".home-still")
            .join("autotune-state.json");
        Self {
            scribe_url: "http://127.0.0.1:7433".into(),
            tick_interval_secs: 1800,
            warmup_secs: 120,
            measure_secs: 1440,
            values: vec![2, 4, 6, 8, 12, 16, 24],
            improvement_threshold: 1.05,
            regression_threshold: 0.90,
            converge_after_stable: 3,
            state_path,
        }
    }
}

fn default_inbox_poll_interval_secs() -> u64 {
    30
}

fn default_convert_timeout_secs() -> u64 {
    900
}

impl Default for ScribeConfig {
    fn default() -> Self {
        Self {
            output_dir: resolve_project_dir().join("markdown"),
            watch_dir: resolve_project_dir().join("papers"),
            corrupted_dir: resolve_project_dir().join("corrupted"),
            catalog_dir: resolve_project_dir().join("catalog"),
            servers: vec!["http://localhost:7433".into()],
            local_server: true,
            inbox_poll_interval_secs: default_inbox_poll_interval_secs(),
            convert_timeout_secs: default_convert_timeout_secs(),
            autotune: AutotuneConfig::default(),
            storage: StorageConfig::default(),
            events: EventBusConfig::default(),
        }
    }
}

impl ScribeConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let config_path = dirs::home_dir()
            .map(|d| d.join(".home-still").join("config.yaml"))
            .unwrap_or_default();

        // Nest defaults under "scribe" key so they merge correctly with YAML
        let defaults = serde_json::json!({
            "scribe": {
                "output_dir": ScribeConfig::default().output_dir,
                "watch_dir": ScribeConfig::default().watch_dir,
                "corrupted_dir": ScribeConfig::default().corrupted_dir,
                "catalog_dir": ScribeConfig::default().catalog_dir,
                "servers": ScribeConfig::default().servers,
                "local_server": true,
                "inbox_poll_interval_secs": default_inbox_poll_interval_secs(),
                "convert_timeout_secs": default_convert_timeout_secs(),
            }
        });
        let figment = Figment::from(Serialized::defaults(defaults))
            .merge(Yaml::file(&config_path))
            .merge(Env::prefixed("HS_SCRIBE_"));

        let storage = figment
            .clone()
            .focus("storage")
            .extract::<StorageConfig>()
            .unwrap_or_default();

        let events = figment
            .clone()
            .focus("events")
            .extract::<EventBusConfig>()
            .unwrap_or_default();

        let mut cfg = figment
            .focus("scribe")
            .extract::<ScribeConfig>()
            .unwrap_or_default();
        cfg.storage = storage;
        cfg.events = events;
        Ok(cfg)
    }

    /// Build the configured storage backend.
    pub fn build_storage(&self) -> anyhow::Result<Arc<dyn Storage>> {
        self.storage.build()
    }

    /// Build the configured event bus.
    pub async fn build_event_bus(&self) -> anyhow::Result<Arc<dyn EventBus>> {
        self.events.build().await
    }
}
