use std::sync::Arc;

use figment::{
    providers::{Env, Serialized},
    Figment,
};
use hs_common::config_file::{ConfigError, ConfigFile};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::hardware_profile::HardwareProfile;
use hs_common::storage::{Storage, StorageConfig};
use serde::{Deserialize, Serialize};

use crate::chunker::ChunkerConfig;
use crate::error::DistillError;

// ── Server Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillServerConfig {
    /// Bind address of `hs-distill-server` (CLI `--host` wins).
    pub host: String,
    /// Listen port of `hs-distill-server` (CLI `--port` wins; `hs serve
    /// distill --port` passes it as `HS_DISTILL_PORT`).
    pub port: u16,
    /// MCP endpoint of the myelin server that stores and searches the
    /// chunks (`myelin-mcp`, streamable HTTP). It must be myelin's full
    /// surface: the server defines its stores there at startup and writes
    /// and forgets documents in them, which the agent surface
    /// (`--serve-agents`) refuses. The stores themselves are not configured
    /// here: they are the registry in `hs_common::stores`.
    pub myelin_url: String,
    pub chunk_max_tokens: usize,
    pub chunk_overlap: usize,
    pub llm_metadata: bool,
    pub metadata_model: String,
    /// Ollama base URL, including the port (`http://host:11434`). Ollama
    /// runs the embedding model behind myelin, so the server asks it where
    /// that model is resident (it must be on the GPU) and, when
    /// `llm_metadata` is true, also calls it for metadata. An unparseable
    /// URL is a startup error.
    pub ollama_url: String,
    /// Upper bound on one call to Ollama, seconds. A stalled Ollama fails
    /// that request instead of hanging it.
    pub ollama_timeout_secs: u64,
}

/// Section of `~/.home-still/config.yaml` holding the distill SERVER's
/// settings ([`DistillServerConfig`]).
pub const SERVER_SECTION: &str = "distill_server";

/// Section holding the client's settings ([`DistillClientConfig`]).
pub const CLIENT_SECTION: &str = "distill";

/// `HS_DISTILL_*` variables that are not config keys. None today: the one
/// the CLI sets for the server it starts (`HS_DISTILL_PORT`) is the `port`
/// key. Kept as the explicit allowlist the warning below honours.
pub const NON_CONFIG_ENV: &[&str] = &[];

/// Layer `defaults`, the `section` of the config file and the `HS_DISTILL_*`
/// environment (later wins; `HS_DISTILL_<KEY>` overrides `<section>.<key>`
/// for the section's top-level keys) and extract `T`. A section that is
/// present but does not fit `T` is an error naming the file, the section
/// and the key. A key in the section that no field has and an
/// `HS_DISTILL_*` variable that sets nothing are warnings, logged once: stale
/// keys are common in deployed configs, so they are not errors.
fn extract_section<T>(
    file: &ConfigFile,
    section: &'static str,
    defaults: &T,
) -> Result<T, ConfigError>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let invalid = |e: serde_json::Error| ConfigError::section(file.path(), section, e);
    let own = serde_json::to_value(defaults).map_err(invalid)?;
    let server = serde_json::to_value(DistillServerConfig::default()).map_err(invalid)?;
    let client = serde_json::to_value(DistillClientConfig::default()).map_err(invalid)?;
    hs_common::config_file::warn_unknown_prefixed_env(
        "HS_DISTILL_",
        &[&server, &client],
        NON_CONFIG_ENV,
    );
    let mut figment = Figment::from(Serialized::default(section, defaults));
    if let Some(from_file) = file.section_json(section)? {
        hs_common::config_file::warn_unknown_keys(file.path(), section, &from_file, &own, &[]);
        figment = figment.merge(Serialized::default(section, from_file));
    }
    figment
        .merge(Env::prefixed("HS_DISTILL_").map(move |key| format!("{section}.{key}").into()))
        .focus(section)
        .extract()
        .map_err(|e| ConfigError::section(file.path(), section, e))
}

impl Default for DistillServerConfig {
    /// The documented built-in defaults. Loading
    /// ([`DistillServerConfig::load`]) layers the config file and the
    /// environment over them.
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 7434,
            myelin_url: "http://127.0.0.1:7462/mcp".into(),
            chunk_max_tokens: 1000,
            chunk_overlap: 100,
            llm_metadata: false,
            metadata_model: "llama3.2:latest".into(),
            ollama_url: "http://localhost:11434".into(),
            ollama_timeout_secs: 120,
        }
    }
}

impl DistillServerConfig {
    /// The server's effective settings: the documented defaults, then the
    /// `distill_server:` section of `~/.home-still/config.yaml`, then
    /// `HS_DISTILL_*` environment variables. Any problem (unreadable or
    /// malformed file or section, a bad env value) is an `Err`; the server
    /// refuses to start on it. A missing file is a valid, empty config.
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::from_file(&ConfigFile::load()?)?)
    }

    /// [`Self::load`] against an already-read config file.
    pub fn from_file(file: &ConfigFile) -> Result<Self, ConfigError> {
        extract_section(file, SERVER_SECTION, &Self::default())
    }

    /// Reject configurations the server cannot run correctly. Call after
    /// [`Self::load`] (or after building a config in code); the server
    /// refuses to start on an `Err`.
    pub fn validate(&self) -> Result<(), DistillError> {
        if self.host.trim().is_empty() {
            return Err(DistillError::Config("host must not be empty".into()));
        }
        if self.port == 0 {
            return Err(DistillError::Config("port must not be 0".into()));
        }

        ChunkerConfig {
            max_tokens: self.chunk_max_tokens,
            overlap_tokens: self.chunk_overlap,
            ..ChunkerConfig::default()
        }
        .validate()?;
        // The embedding model sees a chunk and its header whole or the tail of
        // every chunk is silently cut out of its vector.
        if self
            .chunk_max_tokens
            .saturating_add(CHUNK_HEADER_RESERVE_TOKENS)
            > MODEL_MAX_LENGTH
        {
            return Err(DistillError::Config(format!(
                "chunk_max_tokens ({}) + {CHUNK_HEADER_RESERVE_TOKENS} header tokens exceeds the \
                 embedding model's {MODEL_MAX_LENGTH}-token context: the tail of every chunk \
                 would be truncated out of its vector. Lower chunk_max_tokens",
                self.chunk_max_tokens
            )));
        }

        // Ollama is always used: it runs the embedding model, and the server
        // checks that the model is on the GPU.
        crate::metadata::parse_ollama_url(&self.ollama_url)?;
        if self.ollama_timeout_secs == 0 {
            return Err(DistillError::Config(
                "ollama_timeout_secs must be at least 1".into(),
            ));
        }
        parse_myelin_url(&self.myelin_url)?;
        if self.llm_metadata && self.metadata_model.trim().is_empty() {
            return Err(DistillError::Config(
                "llm_metadata is true but metadata_model is empty".into(),
            ));
        }
        Ok(())
    }
}

/// The myelin MCP endpoint as a URL. An invalid one is an error, never a
/// default.
pub fn parse_myelin_url(myelin_url: &str) -> Result<reqwest::Url, DistillError> {
    let url = reqwest::Url::parse(myelin_url)
        .map_err(|e| DistillError::Config(format!("myelin_url {myelin_url:?} is invalid: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(DistillError::Config(format!(
            "myelin_url {myelin_url:?} must be an http(s) URL with a host"
        )));
    }
    Ok(url)
}

/// The embedding model's context window (bge-m3 under ollama: 8192 tokens).
pub const MODEL_MAX_LENGTH: usize = 8192;

/// Tokens reserved for the contextual header the chunker prepends to every
/// chunk (`"{title} > chunk {n}\n\n"`) and the model's special tokens.
pub const CHUNK_HEADER_RESERVE_TOKENS: usize = 48;

// ── Client Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillClientConfig {
    /// Distill servers. Empty by default: there is no server to guess at;
    /// commands that need one call [`Self::require_servers`].
    pub servers: Vec<String>,
    /// Event-watch worker concurrency — how many markdown documents the
    /// local `hs distill watch-events` loop will index in parallel.
    /// `None` means "use the HardwareProfile default for this host"
    /// (Pi=2, AppleSiliconLow=4, AppleSiliconHigh=6, Nvidia*=8, GenericCpu
    /// scales with `cpu_count/4`). Explicit override wins.
    pub concurrency: Option<usize>,
    /// Deadline for one indexing request, seconds. The event watcher sends
    /// JetStream in-progress heartbeats while a handler runs, so this no
    /// longer has to fit inside `events.nats.ack_wait_secs`. See
    /// `client::DEFAULT_INDEX_TIMEOUT` for the reasoning behind the default.
    pub index_timeout_secs: u64,
    #[serde(skip)]
    pub storage: StorageConfig,
    /// Event bus (top-level `events:` section); `None` when the config has
    /// none, in which case components that publish or consume events refuse
    /// to start ([`Self::build_event_bus`]).
    #[serde(skip)]
    pub events: Option<EventBusConfig>,
}

impl Default for DistillClientConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            concurrency: None,
            index_timeout_secs: crate::client::DEFAULT_INDEX_TIMEOUT.as_secs(),
            storage: StorageConfig::default(),
            events: None,
        }
    }
}

impl DistillClientConfig {
    /// The configured distill servers, or an error telling the operator how
    /// to configure one. Commands that talk to a distill server call this
    /// instead of guessing a `localhost` address.
    pub fn require_servers(&self) -> anyhow::Result<&[String]> {
        if self.servers.is_empty() {
            anyhow::bail!(
                "no distill server is configured: set `distill.servers` in {} \
                 (for example `servers: [http://<host>:7434]`; `hs config init` writes a template)",
                hs_common::CONFIG_REL_PATH
            );
        }
        Ok(&self.servers)
    }
}

impl DistillClientConfig {
    /// Resolve the effective worker concurrency: explicit config value, or
    /// the HardwareProfile default for this host.
    pub fn resolved_concurrency(&self) -> usize {
        self.concurrency.unwrap_or_else(|| {
            let profile = HardwareProfile::detect();
            profile.class.distill_concurrency(profile.cpu_count)
        })
    }

    /// The client's effective settings: the documented defaults, then the
    /// `distill:` section of `~/.home-still/config.yaml`, then
    /// `HS_DISTILL_*` environment variables. `storage:` and `events:` are
    /// read from their own sections. A missing file is a valid, empty
    /// config; a file or section that is present but malformed is an `Err`
    /// naming it.
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::from_file(&ConfigFile::load()?)?)
    }

    /// [`Self::load`] against an already-read config file.
    pub fn from_file(file: &ConfigFile) -> Result<Self, ConfigError> {
        let mut cfg: Self = extract_section(file, CLIENT_SECTION, &Self::default())?;
        cfg.storage = file.storage()?;
        cfg.events = EventBusConfig::from_file(file)?;
        cfg.validate()
            .map_err(|e| ConfigError::section(file.path(), CLIENT_SECTION, e))?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), DistillError> {
        if self.index_timeout_secs == 0 {
            return Err(DistillError::Config(
                "distill.index_timeout_secs must be at least 1".into(),
            ));
        }
        if self.concurrency == Some(0) {
            return Err(DistillError::Config(
                "distill.concurrency must be at least 1 (omit it for the hardware default)".into(),
            ));
        }
        Ok(())
    }

    /// The deadline for one indexing request.
    pub fn index_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.index_timeout_secs)
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Server config validation ───────────────────────────────────────

    fn cfg() -> DistillServerConfig {
        DistillServerConfig::default()
    }

    fn config_error(c: &DistillServerConfig) -> String {
        match c.validate() {
            Err(DistillError::Config(msg)) => msg,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn default_server_config_is_valid() {
        cfg().validate().expect("defaults must validate");
    }

    #[test]
    fn a_chunk_that_does_not_fit_the_model_context_is_rejected() {
        // RA-40: chunks longer than the embedder's window silently lost
        // their back half.
        let mut c = cfg();
        c.chunk_max_tokens = MODEL_MAX_LENGTH - CHUNK_HEADER_RESERVE_TOKENS;
        c.validate().expect("exactly fitting is allowed");
        c.chunk_max_tokens += 1;
        let msg = config_error(&c);
        assert!(msg.contains("8192"), "{msg}");
    }

    #[test]
    fn chunker_sizes_are_validated_with_the_server_config() {
        let mut c = cfg();
        c.chunk_max_tokens = 0;
        config_error(&c);
        let mut c = cfg();
        c.chunk_overlap = c.chunk_max_tokens;
        config_error(&c);
    }

    // ── Loading (RA-6) ─────────────────────────────────────────────────
    //
    // The tests that run `from_file` under a controlled process environment
    // live in `tests/config_load.rs`: setting variables is not safe beside
    // the other threads of this lib test binary.

    fn home_with(yaml: Option<&str>) -> (tempfile::TempDir, ConfigFile) {
        let home = tempfile::tempdir().unwrap();
        if let Some(yaml) = yaml {
            let path = home.path().join(hs_common::CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, yaml).unwrap();
        }
        let file = ConfigFile::load_in(home.path()).unwrap();
        (home, file)
    }

    #[test]
    fn ollama_is_always_validated_and_the_metadata_model_only_when_enabled() {
        // Ollama runs the embedding model: its URL and timeout matter even
        // when metadata extraction is off.
        let mut c = cfg();
        c.ollama_url = "http://localhost:notaport".into();
        config_error(&c);
        let mut c = cfg();
        c.ollama_timeout_secs = 0;
        config_error(&c);

        let mut c = cfg();
        c.metadata_model = "  ".into();
        c.validate().expect("unused while llm_metadata is false");
        c.llm_metadata = true;
        config_error(&c);
    }

    #[test]
    fn the_myelin_url_must_be_an_http_url() {
        for bad in [
            "",
            "not a url",
            "ftp://host/mcp",
            "http://localhost:notaport/mcp",
        ] {
            let mut c = cfg();
            c.myelin_url = bad.into();
            assert!(config_error(&c).contains("myelin_url"), "{bad:?}");
        }
        assert_eq!(cfg().myelin_url, "http://127.0.0.1:7462/mcp");
    }

    // ── Client config ──────────────────────────────────────────────────

    fn client_config(index_timeout_secs: u64) -> DistillClientConfig {
        DistillClientConfig {
            index_timeout_secs,
            ..DistillClientConfig::default()
        }
    }

    #[test]
    fn default_client_config_is_valid() {
        let c = DistillClientConfig::default();
        c.validate().unwrap();
        assert_eq!(c.index_timeout(), crate::client::DEFAULT_INDEX_TIMEOUT);
    }

    #[test]
    fn zero_timeout_or_concurrency_is_rejected() {
        assert!(client_config(0).validate().is_err());
        let c = DistillClientConfig {
            concurrency: Some(0),
            ..DistillClientConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_stale_or_misspelt_key_is_a_warning_naming_the_valid_keys() {
        let (_home, file) = home_with(Some(
            "distill_server:\n  port: 7555\n  myelin_urll: http://x\n  chunk_max_token: 4\ndistill:\n  index_timeout: 5\n",
        ));
        let tree = serde_json::to_value(DistillServerConfig::default()).unwrap();
        let section = file.section_json(SERVER_SECTION).unwrap().unwrap();
        let found: Vec<String> = hs_common::config_file::unknown_key_notices(
            file.path(),
            SERVER_SECTION,
            &section,
            &tree,
            &[],
        )
        .into_iter()
        .collect();
        // The two typos are reported, naming the section and the valid keys.
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .any(|n| n.contains("distill_server.myelin_urll")));
        assert!(found.iter().any(
            |n| n.contains("distill_server.chunk_max_token") && n.contains("chunk_max_tokens")
        ));
    }

    /// Every `HS_DISTILL_*` variable the README or the user docs tell an
    /// operator about must set a real key (or be allow-listed).
    #[test]
    fn every_documented_variable_maps_to_a_field() {
        let documents = [
            ("README.md", include_str!("../README.md")),
            (
                "docs/reference/configuration.md",
                include_str!("../../../docs/reference/configuration.md"),
            ),
            (
                "docs/tutorials/home-cloud-cluster.md",
                include_str!("../../../docs/tutorials/home-cloud-cluster.md"),
            ),
        ];
        let server = serde_json::to_value(DistillServerConfig::default()).unwrap();
        let client = serde_json::to_value(DistillClientConfig::default()).unwrap();
        let mut checked = 0;
        for (name, text) in documents {
            for var in hs_common::config_file::env_names_in_text(text, "HS_DISTILL_") {
                checked += 1;
                let unknown = hs_common::config_file::unknown_prefixed_env(
                    "HS_DISTILL_",
                    &[&server, &client],
                    NON_CONFIG_ENV,
                    std::iter::once(var.clone()),
                );
                assert!(
                    unknown.is_empty(),
                    "{name} documents {var}, which sets nothing"
                );
            }
        }
        assert!(checked >= 1, "the scan found no variables");
    }
}
