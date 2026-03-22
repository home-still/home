use crate::resilience::config::ResilienceConfig;
use anyhow::Context;
use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resilience: ResilienceConfig::default(),
            download_path: dirs::home_dir()
                .map(|h| h.join("Downloads/home-still/papers"))
                .unwrap_or_else(|| PathBuf::from("./downloads")),
            cache_path: dirs::home_dir()
                .map(|h| h.join(".home-still/paper/cache"))
                .unwrap_or_else(|| PathBuf::from("./cache")),
            providers: ProvidersConfig::default(),
            download: DownloadConfig::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".home-still/config.yaml"))
    }

    pub fn load() -> anyhow::Result<Self> {
        let mut figment = Figment::new();

        let system_path = PathBuf::from("/etc/home-still/config.yaml");
        if system_path.exists() {
            figment = figment.merge(Yaml::file(&system_path));
        }

        if let Some(home) = dirs::home_dir() {
            let user_path = home.join(".home-still/config.yaml");
            if user_path.exists() {
                figment = figment.merge(Yaml::file(&user_path));
            }
        }

        figment = figment.merge(Env::prefixed("HOME_STILL_").split("_"));

        let config: Config = figment
            .focus("paper")
            .merge(Serialized::defaults(Config::default()))
            .extract()
            .context("Failed to load configuration")?;

        Ok(config)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    pub arxiv: ArxivConfig,
    pub openalex: OpenAlexConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Maximum concurrent downloads
    pub max_concurrent: usize,
    /// Per-file download timeout in seconds
    pub timeout_secs: u64,
    /// Unpaywall email address
    pub unpaywall_email: Option<String>,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            timeout_secs: 120,
            unpaywall_email: None,
        }
    }
}
