use crate::error::PaperError;
use crate::resilience::config::ResilienceConfig;
use anyhow::Context;
use figment::{
    providers::{Env, Serialized},
    Figment,
};
use hs_common::config_file::{env_key_path, unknown_env_names, ConfigError, ConfigFile};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::storage::{Storage, StorageConfig};
use hs_common::CONFIG_REL_PATH;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The system-wide config file, merged under the user's.
const SYSTEM_CONFIG_PATH: &str = "/etc/home-still/config.yaml";

/// Prefix of the environment variables that override config keys.
const ENV_PREFIX: &str = "HOME_STILL_";

/// The config key tree, derived from the defaults of the three sections this
/// crate reads (`paper`, `storage`, `events`). It is the single list of valid
/// keys: env overrides are matched against it and nothing else.
fn known_keys() -> anyhow::Result<serde_json::Value> {
    let mut root = serde_json::Map::new();
    root.insert(
        "paper".into(),
        serde_json::to_value(Config::default()).context("serialize paper config defaults")?,
    );
    root.insert(
        "storage".into(),
        serde_json::to_value(StorageConfig::default()).context("serialize storage defaults")?,
    );
    root.insert(
        "events".into(),
        serde_json::to_value(EventBusConfig::noop()).context("serialize events defaults")?,
    );
    Ok(serde_json::Value::Object(root))
}

/// A `HOME_STILL_PAPER_*` variable that matches no `paper.*` key is a typo
/// that would otherwise be silently ignored.
fn reject_unknown_paper_env(tree: &serde_json::Value) -> anyhow::Result<()> {
    let unknown = unknown_env_names(
        ENV_PREFIX,
        &format!("{ENV_PREFIX}PAPER_"),
        tree,
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
    );
    if unknown.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "environment variable(s) {} name no paper config key (words in a key are joined by `_`, \
         e.g. HOME_STILL_PAPER_DOWNLOAD_TIMEOUT_SECS); fix or unset them",
        unknown.join(", ")
    )
}

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

    /// Event bus (noop or NATS). Loaded from the top-level `events:`
    /// section; `None` when the config has none, in which case components
    /// that publish events refuse to start ([`Config::build_event_bus`]).
    #[serde(skip)]
    pub events: Option<EventBusConfig>,
}

impl Default for Config {
    /// The documented built-in defaults, rooted at `~/home-still`. Loading
    /// ([`Config::load`]) roots them at `home.project_dir` and never falls
    /// back to this value.
    fn default() -> Self {
        Self {
            resilience: ResilienceConfig::default(),
            download_path: hs_common::default_project_dir().join("papers"),
            cache_path: hs_common::hidden_dir()
                .unwrap_or_else(|e| panic!("{e}"))
                .join("cache"),
            providers: ProvidersConfig::default(),
            download: DownloadConfig::default(),
            storage: StorageConfig::default(),
            events: None,
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(CONFIG_REL_PATH))
    }

    /// Load the effective configuration: system file, user file, then
    /// `HOME_STILL_*` environment overrides, validated.
    ///
    /// Environment keys name the config path with `_` between *words as well
    /// as levels*: `HOME_STILL_PAPER_DOWNLOAD_PATH` is `paper.download_path`,
    /// `HOME_STILL_PAPER_DOWNLOAD_TIMEOUT_SECS` is `paper.download.timeout_secs`.
    /// A blanket `.split("_")` cannot express that (it turns
    /// `…_TIMEOUT_SECS` into `timeout.secs`), so each variable is matched
    /// against the known key tree instead; see [`env_key_path`]. A
    /// `HOME_STILL_PAPER_*` variable that names no key is an error.
    pub fn load() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir)?;
        let user = ConfigFile::load_in(&home)?;
        let system = ConfigFile::load_at(Path::new(SYSTEM_CONFIG_PATH), &home)?;
        Self::load_from(&system, &user)
    }

    /// [`Self::load`] against already-read config files: the system file
    /// under the user file under the environment. A file or section that is
    /// present but malformed is an error naming it; an absent `storage:`
    /// section is the documented local-filesystem default, an absent
    /// `events:` section is `None` (see [`Self::build_event_bus`]).
    pub fn load_from(system: &ConfigFile, user: &ConfigFile) -> anyhow::Result<Self> {
        // The default for the one key that depends on `home.project_dir`.
        let mut figment = Figment::new().merge(Serialized::default(
            "paper.download_path",
            user.project_dir()?.join("papers"),
        ));
        let keys = Arc::new(known_keys()?);
        for file in [system, user] {
            for section in ["paper", "storage", "events"] {
                if let Some(value) = file.section_json(section)? {
                    if section == "paper" {
                        // A stale or misspelt `paper:` key is ignored; say so.
                        hs_common::config_file::warn_unknown_keys(
                            file.path(),
                            section,
                            &value,
                            &keys[section],
                            &[],
                        );
                    }
                    figment = figment.merge(Serialized::default(section, value));
                }
            }
        }

        reject_unknown_paper_env(&keys)?;
        let env_keys = Arc::clone(&keys);
        figment = figment.merge(
            Env::prefixed(ENV_PREFIX)
                .filter_map(move |key| env_key_path(&env_keys, key.as_str()).map(Into::into)),
        );

        let shown = user.path().display();
        let mut config: Config =
            figment.clone().focus("paper").extract().with_context(|| {
                format!("Failed to parse config ({shown}).  Run: hs config init")
            })?;

        config.storage = if figment.contains("storage") {
            figment
                .clone()
                .focus("storage")
                .extract::<StorageConfig>()
                .with_context(|| format!("{shown}: invalid `storage` section"))?
        } else {
            user.default_storage()?
        };

        config.events = if figment.contains("events") {
            let events = figment
                .focus("events")
                .extract::<EventBusConfig>()
                .with_context(|| format!("{shown}: invalid `events` section"))?;
            events
                .validate()
                .map_err(|e| anyhow::anyhow!("{shown}: invalid `events` section: {e}"))?;
            Some(events)
        } else {
            None
        };

        config.download_path = expand_tilde(&config.download_path);
        config.cache_path = expand_tilde(&config.cache_path);

        config.validate()?;
        Ok(config)
    }

    /// Reject settings that would panic, hang or send credentials in clear
    /// later. Run by [`Config::load`]; call it on a hand-built `Config` too.
    pub fn validate(&self) -> Result<(), PaperError> {
        self.resilience.validate()?;
        self.download.validate()?;
        self.providers.validate()
    }

    /// Build the configured storage backend.
    pub fn build_storage(&self) -> anyhow::Result<Arc<dyn Storage>> {
        self.storage.build()
    }

    /// Build the configured event bus. A config without an `events:` section
    /// has none: this is an error, never a bus that drops everything.
    pub async fn build_event_bus(&self) -> anyhow::Result<Arc<dyn EventBus>> {
        EventBusConfig::build_required(self.events.as_ref()).await
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
            base_url: String::from("https://export.arxiv.org/api/query"),
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
            base_url: String::from("https://api.openalex.org"),
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
    /// Longest a `Retry-After` (or default 429 backoff) is allowed to make a
    /// request sleep, in seconds (>= 1). A larger server directive is
    /// clamped to this; the 429 then surfaces as `RateLimited` with the
    /// server's real `retry_after`.
    pub max_retry_after_secs: u64,
}

impl Default for SemanticScholarConfig {
    fn default() -> Self {
        Self {
            base_url: String::from("https://api.semanticscholar.org"),
            api_key: None,
            timeout_secs: 30,
            rate_limit_interval_ms: 1100, // just over 1 req/s
            max_retry_after_secs: 30,
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

impl ProvidersConfig {
    /// Every provider needs a usable base URL, a non-zero timeout and a
    /// non-zero rate-limit interval (`governor` panics on a zero period),
    /// and a credential may only be configured for an `https` endpoint.
    pub fn validate(&self) -> Result<(), PaperError> {
        let p = self;
        check_provider(
            "arxiv",
            &p.arxiv.base_url,
            p.arxiv.timeout_secs,
            p.arxiv.rate_limit_interval_ms,
            false,
        )?;
        check_provider(
            "openalex",
            &p.openalex.base_url,
            p.openalex.timeout_secs,
            p.openalex.rate_limit_interval_ms,
            p.openalex.api_key.is_some(),
        )?;
        check_provider(
            "semantic_scholar",
            &p.semantic_scholar.base_url,
            p.semantic_scholar.timeout_secs,
            p.semantic_scholar.rate_limit_interval_ms,
            p.semantic_scholar.api_key.is_some(),
        )?;
        if p.semantic_scholar.max_retry_after_secs == 0 {
            return Err(invalid(
                "providers.semantic_scholar.max_retry_after_secs must be at least 1",
            ));
        }
        check_provider(
            "europe_pmc",
            &p.europe_pmc.base_url,
            p.europe_pmc.timeout_secs,
            p.europe_pmc.rate_limit_interval_ms,
            false,
        )?;
        check_provider(
            "crossref",
            &p.crossref.base_url,
            p.crossref.timeout_secs,
            p.crossref.rate_limit_interval_ms,
            false,
        )?;
        if let Some(mailto) = &p.crossref.mailto {
            validate_contact_email("providers.crossref.mailto", mailto)?;
        }
        check_provider(
            "core",
            &p.core.base_url,
            p.core.timeout_secs,
            p.core.rate_limit_interval_ms,
            p.core.api_key.is_some(),
        )?;
        Ok(())
    }
}

fn invalid(msg: impl Into<String>) -> PaperError {
    PaperError::InvalidInput(format!("paper.{}", msg.into()))
}

fn check_provider(
    name: &str,
    base_url: &str,
    timeout_secs: u64,
    rate_limit_interval_ms: u64,
    has_credential: bool,
) -> Result<(), PaperError> {
    let url = url::Url::parse(base_url).map_err(|e| {
        invalid(format!(
            "providers.{name}.base_url {base_url:?} is not a valid URL ({e})"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(invalid(format!(
            "providers.{name}.base_url must be an http(s) URL with a host (got {base_url:?})"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(format!(
            "providers.{name}.base_url must not embed credentials"
        )));
    }
    if has_credential && url.scheme() != "https" && !is_loopback_host(&url) {
        return Err(invalid(format!(
            "providers.{name}.base_url is plain http but an API key is configured; \
             use https so the key is not sent in clear"
        )));
    }
    if timeout_secs == 0 {
        return Err(invalid(format!(
            "providers.{name}.timeout_secs must be at least 1"
        )));
    }
    if rate_limit_interval_ms == 0 {
        return Err(invalid(format!(
            "providers.{name}.rate_limit_interval_ms must be at least 1"
        )));
    }
    Ok(())
}

fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// Maximum concurrent downloads (>= 1)
    pub max_concurrent: usize,
    /// Per-file download timeout in seconds (>= 1)
    pub timeout_secs: u64,
    /// Contact email sent to Unpaywall (which requires one) and, when set,
    /// to NCBI's ID converter (which only asks for one); also appended to
    /// the User-Agent. Without it the Unpaywall source is skipped and says so
    /// in the per-source outcome list; the PMC source is queried without an
    /// `email` parameter rather than with an invented address.
    pub unpaywall_email: Option<String>,
    /// Storage prefix PDFs / HTML / EPUBs are written under. Must match the
    /// prefix `hs status`, `catalog_repair`, and `scribe_convert` read from —
    /// otherwise downloads land out of view of the pipeline. Default
    /// `"papers"`. The pre-rc.298 downloader omitted this prefix entirely
    /// and scattered files across bucket-root shards (`00/`, `W2/`, …);
    /// `hs repair move-root-orphans` relocates them.
    pub papers_prefix: String,
    /// Hard cap on one downloaded body, in bytes. Enforced while streaming:
    /// the transfer is aborted the moment it is exceeded, and the remote
    /// `Content-Length` is never trusted to size a buffer. The default
    /// equals the scribe upload limit (256 MiB), so anything this accepts
    /// can also be converted.
    pub max_download_bytes: u64,
}

/// Default for [`DownloadConfig::max_download_bytes`].
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            timeout_secs: 120,
            unpaywall_email: None,
            papers_prefix: "papers".to_string(),
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
        }
    }
}

impl DownloadConfig {
    /// Reject values that would hang, panic or write to an unusable place
    /// later. Run by [`Config::load`] and by the downloader's constructor, so
    /// a config built in code is held to the same rules as one read from
    /// YAML.
    pub fn validate(&self) -> Result<(), PaperError> {
        let fail = |msg: String| Err(PaperError::InvalidInput(format!("paper.download.{msg}")));
        if self.max_concurrent == 0 {
            return fail(
                "max_concurrent must be at least 1 (0 would never start a download)".into(),
            );
        }
        if self.timeout_secs == 0 {
            return fail("timeout_secs must be at least 1".into());
        }
        let min = crate::providers::downloader::MIN_PDF_BYTES;
        if self.max_download_bytes < min {
            return fail(format!(
                "max_download_bytes must be at least {min} (the smallest PDF accepted)"
            ));
        }
        if self.papers_prefix.trim_end_matches('/').is_empty() {
            return fail("papers_prefix must not be empty".into());
        }
        if let Err(e) = hs_common::storage::validate_prefix(&self.papers_prefix) {
            return fail(format!("papers_prefix is not a valid storage prefix: {e}"));
        }
        if let Some(email) = &self.unpaywall_email {
            validate_contact_email("unpaywall_email", email)?;
        }
        Ok(())
    }
}

/// A contact address goes into a User-Agent and into query strings: it must
/// look like one address, with no whitespace or control characters.
fn validate_contact_email(key: &str, email: &str) -> Result<(), PaperError> {
    let ok = !email.is_empty()
        && email.contains('@')
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(PaperError::InvalidInput(format!(
            "{key} must be a single email address (got {email:?})"
        )))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_use_https() {
        let config = Config::default();
        config.validate().unwrap();
        let p = &config.providers;
        for url in [
            &p.arxiv.base_url,
            &p.openalex.base_url,
            &p.semantic_scholar.base_url,
            &p.europe_pmc.base_url,
            &p.crossref.base_url,
            &p.core.base_url,
        ] {
            assert!(url.starts_with("https://"), "{url}");
        }
    }

    #[test]
    fn values_that_panic_or_hang_later_are_rejected_at_load_time() {
        let mut cases: Vec<(&str, Config)> = Vec::new();
        let mut c = Config::default();
        c.providers.crossref.rate_limit_interval_ms = 0; // governor panics
        cases.push(("zero rate limit interval", c));
        let mut c = Config::default();
        c.download.max_concurrent = 0; // buffer_unordered(0) hangs
        cases.push(("zero max_concurrent", c));
        let mut c = Config::default();
        c.download.timeout_secs = 0;
        cases.push(("zero download timeout", c));
        let mut c = Config::default();
        c.providers.arxiv.timeout_secs = 0;
        cases.push(("zero provider timeout", c));
        let mut c = Config::default();
        c.resilience.cb_initial_backoff_secs = 0; // failsafe asserts
        cases.push(("zero breaker backoff", c));
        let mut c = Config::default();
        c.providers.openalex.base_url = "not a url".into();
        cases.push(("bad base_url", c));
        let mut c = Config::default();
        c.providers.openalex.base_url = "ftp://api.openalex.org".into();
        cases.push(("non-http base_url", c));
        let mut c = Config::default();
        c.download.unpaywall_email = Some("not an email".into());
        cases.push(("bad email", c));
        let mut c = Config::default();
        c.download.papers_prefix = "/".into();
        cases.push(("empty papers prefix", c));
        for (name, config) in cases {
            assert!(config.validate().is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn an_api_key_is_never_sent_over_plain_http_except_to_loopback() {
        let mut c = Config::default();
        c.providers.openalex.base_url = "http://api.openalex.org".into();
        c.providers.openalex.api_key = Some("k".into());
        assert!(c.validate().is_err());
        // Without a key, plain http is allowed (it was the old default).
        c.providers.openalex.api_key = None;
        c.validate().unwrap();
        // A local mirror may use http with a key.
        c.providers.openalex.base_url = "http://127.0.0.1:9000".into();
        c.providers.openalex.api_key = Some("k".into());
        c.validate().unwrap();
    }

    #[test]
    fn env_paths_resolve_the_way_words_group() {
        let tree = known_keys().unwrap();
        let path = |n: &str| env_key_path(&tree, n);
        assert_eq!(
            path("PAPER_DOWNLOAD_PATH").as_deref(),
            Some("paper.download_path")
        );
        assert_eq!(
            path("PAPER_DOWNLOAD_TIMEOUT_SECS").as_deref(),
            Some("paper.download.timeout_secs")
        );
        assert_eq!(
            path("paper_providers_europe_pmc_base_url").as_deref(),
            Some("paper.providers.europe_pmc.base_url")
        );
        assert_eq!(path("PAPER_DOWNLOAD"), None, "a section is not a value");
        assert_eq!(path("PAPER_NOPE"), None);
        assert_eq!(path("COLOR"), None);
    }
}
