use crate::resilience::config::ResilienceConfig;
use anyhow::Context;
use figment::{
    providers::{Env, Format, Yaml},
    Figment,
};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::storage::{Storage, StorageConfig};
use hs_common::CONFIG_REL_PATH;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Main application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Resilience patterns configuration
    pub resilience: ResilienceConfig,

    /// Directory where downloaded papers are stored
    pub download_path: PathBuf,

    /// Directory for caching metadata and search results
    pub cache_path: PathBuf,

    /// Paper providers
    pub providers: ProvidersConfig,

    /// Download config
    pub download: DownloadConfig,

    /// Storage backend (local filesystem or Garage/S3).
    /// Loaded from the top-level `storage:` section of the config file.
    #[serde(skip)]
    pub storage: StorageConfig,

    /// Event bus (noop or NATS). Loaded from top-level `events:` section.
    #[serde(skip)]
    pub events: EventBusConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resilience: ResilienceConfig::default(),
            download_path: hs_common::resolve_project_dir().join("papers"),
            cache_path: dirs::home_dir()
                .map(|h| h.join(hs_common::HIDDEN_DIR).join("cache"))
                .unwrap_or_else(|| PathBuf::from("./cache")),
            providers: ProvidersConfig::default(),
            download: DownloadConfig::default(),
            storage: StorageConfig::default(),
            events: EventBusConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(CONFIG_REL_PATH))
    }

    pub fn load() -> anyhow::Result<Self> {
        let mut figment = Figment::new();

        let system_path = PathBuf::from("/etc/home-still/config.yaml");
        if system_path.exists() {
            figment = figment.merge(Yaml::file(&system_path));
        }

        if let Some(home) = dirs::home_dir() {
            let user_path = home.join(CONFIG_REL_PATH);
            if user_path.exists() {
                figment = figment.merge(Yaml::file(&user_path));
            }
        }

        figment = figment.merge(Env::prefixed("HOME_STILL_").split("_"));

        let mut config: Config = figment.clone().focus("paper").extract().context(format!(
            "Failed to parse config ({}).  Run: hs config init",
            Config::config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| CONFIG_REL_PATH.into())
        ))?;

        config.storage = figment
            .clone()
            .focus("storage")
            .extract::<StorageConfig>()
            .unwrap_or_default();

        config.events = figment
            .focus("events")
            .extract::<EventBusConfig>()
            .unwrap_or_default();

        config.download_path = expand_tilde(&config.download_path);
        config.cache_path = expand_tilde(&config.cache_path);

        Ok(config)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArxivConfig {
    pub base_url: String,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for ArxivConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("http://export.arxiv.org/api/query"),
            timeout_secs: 30,
            rate_limit_interval_ms: 3000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAlexConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for OpenAlexConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("http://api.openalex.org"),
            api_key: None,
            timeout_secs: 30,
            rate_limit_interval_ms: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticScholarConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for SemanticScholarConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("https://api.semanticscholar.org"),
            api_key: None,
            timeout_secs: 30,
            rate_limit_interval_ms: 1100, // just over 1 req/s
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EuropePmcConfig {
    pub base_url: String,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for EuropePmcConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("https://www.ebi.ac.uk/europepmc"),
            timeout_secs: 30,
            rate_limit_interval_ms: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossRefConfig {
    pub base_url: String,
    pub mailto: Option<String>,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for CrossRefConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("https://api.crossref.org"),
            mailto: None,
            timeout_secs: 30,
            rate_limit_interval_ms: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    pub rate_limit_interval_ms: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("https://api.core.ac.uk"),
            api_key: None,
            timeout_secs: 30,
            rate_limit_interval_ms: 2100, // 5 req/10s
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProvidersConfig {
    pub arxiv: ArxivConfig,
    pub openalex: OpenAlexConfig,
    pub semantic_scholar: SemanticScholarConfig,
    pub europe_pmc: EuropePmcConfig,
    pub crossref: CrossRefConfig,
    pub core: CoreConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// Maximum concurrent downloads
    pub max_concurrent: usize,
    /// Per-file download timeout in seconds
    pub timeout_secs: u64,
    /// Unpaywall email address
    pub unpaywall_email: Option<String>,
    /// Storage prefix PDFs / HTML / EPUBs are written under. Must match the
    /// prefix `hs status`, `catalog_repair`, and `scribe_convert` read from —
    /// otherwise downloads land out of view of the pipeline. Default
    /// `"papers"`. The pre-rc.298 downloader omitted this prefix entirely
    /// and scattered files across bucket-root shards (`00/`, `W2/`, …);
    /// `hs repair move-root-orphans` relocates them.
    pub papers_prefix: String,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            timeout_secs: 120,
            unpaywall_email: None,
            papers_prefix: "papers".to_string(),
        }
    }
}

fn expand_tilde(path: &std::path::Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}
