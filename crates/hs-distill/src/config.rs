use std::path::PathBuf;
use std::sync::Arc;

use figment::{providers::{Env, Serialized}, Figment};
use hs_common::config_file::{ConfigError, ConfigFile};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::hardware_profile::HardwareProfile;
use hs_common::storage::{Storage, StorageConfig};
use serde::{Deserialize, Serialize};

use crate::chunker::ChunkerConfig;
use crate::error::DistillError;

/// Compute device for embedding inference. rc.306 P0-7: CUDA is the
/// only accepted value — the distill binary ships with no CPU code path.
/// Attempting to load a config with `compute_device: cpu` (or anything
/// other than `cuda`) fails deserialization loudly.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ComputeDevice {
    #[default]
    Cuda,
}

impl std::fmt::Display for ComputeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComputeDevice::Cuda => write!(f, "Cuda"),
        }
    }
}

// ── Server Config ──────────────────────────────────────────────

/// Collections requests may name besides `collection_name`, unless the
/// config says otherwise. `paper_abstracts` is written by
/// `hs distill abstracts` and read by the MCP `abstract_search` tool;
/// `personal_docs` is the `hs personal` store (`personal.collection_name`).
const DEFAULT_EXTRA_COLLECTIONS: [&str; 2] = ["paper_abstracts", "personal_docs"];

/// Config keys that no longer exist. They deserialize into nothing, so
/// [`DistillServerConfig::load`] names any that are still set instead of
/// letting an operator believe they have an effect.
const REMOVED_KEYS: [&str; 2] = ["embedding.model", "embedding.sparse_enabled"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillServerConfig {
    /// Bind address of `hs-distill-server` (CLI `--host` wins).
    pub host: String,
    /// Listen port of `hs-distill-server` (CLI `--port` wins; `hs serve
    /// distill --port` passes it as `HS_DISTILL_PORT`).
    pub port: u16,
    pub qdrant_url: String,
    pub qdrant_data_dir: PathBuf,
    /// The default collection: requests that name none use it.
    pub collection_name: String,
    /// Further collections requests may name. Every configured collection
    /// is created/verified at startup; a request naming any other
    /// collection is rejected (HTTP 400), so a caller can never create or
    /// reset a collection by naming it.
    pub collections: Vec<String>,
    pub embedding: EmbeddingConfig,
    /// HNSW parameters for new collections and the search-time `ef`.
    pub hnsw: HnswConfig,
    pub chunk_max_tokens: usize,
    pub chunk_overlap: usize,
    /// Number of chunks per Qdrant upsert request. Each chunk carries a
    /// 1024-dim f32 dense vector plus payload (~3–5 KB), so the default
    /// 1000 sits well within Qdrant's 4 MB gRPC frame limit while
    /// amortizing per-request overhead.
    pub qdrant_upsert_batch: usize,
    /// How many Qdrant upsert requests to fire in parallel per document.
    /// Qdrant handles many concurrent writes to one collection cheaply,
    /// so this keeps the upsert phase from being the slow link after a
    /// fast embed.
    pub qdrant_upsert_parallelism: usize,
    pub llm_metadata: bool,
    pub metadata_model: String,
    /// Ollama base URL, including the port (`http://host:11434`). Only used
    /// when `llm_metadata` is true; an unparseable URL is a startup error.
    pub ollama_url: String,
    /// Upper bound on one metadata-model call, seconds. A stalled Ollama
    /// fails that document's indexing instead of hanging it.
    pub ollama_timeout_secs: u64,
}

/// Section of `~/.home-still/config.yaml` holding the distill SERVER's
/// settings ([`DistillServerConfig`]).
pub const SERVER_SECTION: &str = "distill_server";

/// Section holding the client's settings ([`DistillClientConfig`]).
pub const CLIENT_SECTION: &str = "distill";

/// Layer `defaults`, the `section` of the config file and the `HS_DISTILL_*`
/// environment (later wins; `HS_DISTILL_<KEY>` overrides `<section>.<key>`
/// for the section's top-level keys) and extract `T`. A section that is
/// present but does not fit `T` is an error naming the file, the section
/// and the key.
fn extract_section<T>(
    file: &ConfigFile,
    section: &'static str,
    defaults: &T,
) -> Result<T, ConfigError>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let mut figment = Figment::from(Serialized::default(section, defaults));
    if let Some(from_file) = file.section_json(section)? {
        figment = figment.merge(Serialized::default(section, from_file));
    }
    figment
        .merge(Env::prefixed("HS_DISTILL_").map(move |key| format!("{section}.{key}").into()))
        .focus(section)
        .extract()
        .map_err(|e| ConfigError::section(file.path(), section, e))
}

impl Default for DistillServerConfig {
    /// The documented built-in defaults, rooted at `~/home-still`. Loading
    /// ([`DistillServerConfig::load`]) roots them at the configured
    /// `home.project_dir` and never falls back to this value.
    fn default() -> Self {
        Self::with_project_dir(&hs_common::default_project_dir())
    }
}

impl DistillServerConfig {
    /// Defaults with the Qdrant data directory under `project`
    /// (`home.project_dir`).
    pub fn with_project_dir(project: &std::path::Path) -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 7434,
            qdrant_url: "http://localhost:6334".into(),
            qdrant_data_dir: project.join("data").join("qdrant"),
            collection_name: "academic_papers".into(),
            collections: DEFAULT_EXTRA_COLLECTIONS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            embedding: EmbeddingConfig::default(),
            hnsw: HnswConfig::default(),
            chunk_max_tokens: 1000,
            chunk_overlap: 100,
            qdrant_upsert_batch: 1000,
            qdrant_upsert_parallelism: 4,
            llm_metadata: false,
            metadata_model: "llama3.2:latest".into(),
            ollama_url: "http://localhost:11434".into(),
            ollama_timeout_secs: 120,
        }
    }

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
        let defaults = Self::with_project_dir(&file.project_dir()?);
        if let Some(section) = file.section_json(SERVER_SECTION)? {
            for key in removed_keys_present(&section) {
                tracing::warn!(
                    key = %format!("{SERVER_SECTION}.{key}"),
                    "config key no longer exists and is ignored; remove it"
                );
            }
        }
        extract_section(file, SERVER_SECTION, &defaults)
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

        validate_collection_name(&self.collection_name)?;
        let mut seen = vec![self.collection_name.as_str()];
        for name in &self.collections {
            validate_collection_name(name)?;
            if seen.contains(&name.as_str()) {
                return Err(DistillError::Config(format!(
                    "collection {name:?} is listed twice (collection_name + collections)"
                )));
            }
            seen.push(name);
        }

        ChunkerConfig {
            max_tokens: self.chunk_max_tokens,
            overlap_tokens: self.chunk_overlap,
            ..ChunkerConfig::default()
        }
        .validate()?;
        self.embedding.validate(self.chunk_max_tokens)?;
        self.hnsw.validate()?;

        if self.qdrant_upsert_batch == 0 {
            return Err(DistillError::Config(
                "qdrant_upsert_batch must be at least 1".into(),
            ));
        }
        if self.qdrant_upsert_parallelism == 0 {
            return Err(DistillError::Config(
                "qdrant_upsert_parallelism must be at least 1".into(),
            ));
        }

        if self.llm_metadata {
            crate::metadata::parse_ollama_url(&self.ollama_url)?;
            if self.metadata_model.trim().is_empty() {
                return Err(DistillError::Config(
                    "llm_metadata is true but metadata_model is empty".into(),
                ));
            }
            if self.ollama_timeout_secs == 0 {
                return Err(DistillError::Config(
                    "ollama_timeout_secs must be at least 1".into(),
                ));
            }
        }
        Ok(())
    }

    /// Every collection the server serves: the default first, then
    /// `collections`.
    pub fn served_collections(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.collection_name.as_str())
            .chain(self.collections.iter().map(String::as_str))
    }
}

/// A Qdrant collection name this server will accept: 1–64 chars of
/// `[A-Za-z0-9_-]`, starting with a letter or digit.
pub fn validate_collection_name(name: &str) -> Result<(), DistillError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(DistillError::Config(format!(
            "collection name {name:?} must be 1-64 chars of [A-Za-z0-9_-] starting with a letter or digit"
        )))
    }
}

/// The [`REMOVED_KEYS`] (dotted paths) still set in `section`.
fn removed_keys_present(section: &serde_json::Value) -> Vec<&'static str> {
    REMOVED_KEYS
        .iter()
        .copied()
        .filter(|key| {
            key.split('.')
                .try_fold(section, |node, part| node.get(part))
                .is_some()
        })
        .collect()
}

/// HNSW graph parameters. Applied when a collection is created; existing
/// collections keep whatever they were built with (see
/// `qdrant::ensure_collection`, which reports a collection whose HNSW is
/// disabled). `search_ef` applies to every query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HnswConfig {
    /// Edges per node. 0 would disable the index (what every collection
    /// created before RA-41 has); the minimum accepted is 4.
    pub m: u64,
    /// Candidates considered while building the graph.
    pub ef_construct: u64,
    /// Candidates considered per query.
    pub search_ef: u64,
    /// Threads Qdrant may use to build the graph when HNSW is enabled on an
    /// existing collection (`POST /collection/hnsw`). Bounded so the index
    /// build cannot starve the other tenants of a shared host.
    pub max_indexing_threads: u64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        // Qdrant's own defaults for m / ef_construct; search_ef is the
        // value the search path has always requested.
        Self {
            m: 16,
            ef_construct: 100,
            search_ef: 128,
            max_indexing_threads: 4,
        }
    }
}

impl HnswConfig {
    pub fn validate(&self) -> Result<(), DistillError> {
        if !(4..=128).contains(&self.m) {
            return Err(DistillError::Config(format!(
                "hnsw.m must be 4..=128 (got {}); 0 would disable the index",
                self.m
            )));
        }
        if !(self.m..=1024).contains(&self.ef_construct) {
            return Err(DistillError::Config(format!(
                "hnsw.ef_construct must be between hnsw.m ({}) and 1024 (got {})",
                self.m, self.ef_construct
            )));
        }
        if !(1..=64).contains(&self.max_indexing_threads) {
            return Err(DistillError::Config(format!(
                "hnsw.max_indexing_threads must be 1..=64 (got {})",
                self.max_indexing_threads
            )));
        }
        if !(1..=4096).contains(&self.search_ef) {
            return Err(DistillError::Config(format!(
                "hnsw.search_ef must be 1..=4096 (got {})",
                self.search_ef
            )));
        }
        Ok(())
    }
}

/// bge-m3's context window; the tokenizer cannot be asked for more.
pub const MODEL_MAX_LENGTH: usize = 8192;

/// Tokens reserved for the contextual header the chunker prepends to every
/// chunk (`"{title} > chunk {n}\n\n"`) and the model's special tokens.
pub const CHUNK_HEADER_RESERVE_TOKENS: usize = 48;

/// Largest batch (rows per forward pass) that keeps the worst-case
/// self-attention scratch at or below what the embedder has always allowed:
/// 128 rows x 512 tokens. That scratch grows with `rows x length^2`, so a
/// longer `max_length` shrinks the batch quadratically. The card is shared
/// with the scribe VLM and ollama, and a CUDA OOM poisons the ort session.
pub fn max_batch_rows(max_length: usize) -> usize {
    const HISTORICAL_ROWS_X_LEN_SQ: usize = 128 * 512 * 512;
    (HISTORICAL_ROWS_X_LEN_SQ / (max_length.max(1) * max_length.max(1))).max(1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Expected embedding width. The model fixes it (bge-m3: 1024); the
    /// server compares this value with what the model actually returns at
    /// startup and refuses to run on a mismatch, so a collection can never
    /// be created with the wrong vector size.
    pub dimension: usize,
    /// Tokens the model sees per text; longer input is truncated by the
    /// tokenizer. Must cover a whole chunk: `chunk_max_tokens + 48` must
    /// not exceed it. The default (1280) holds the default 1000-token chunk
    /// plus its header with ~25% slack for the chars/4 token estimate.
    /// Raising it costs VRAM per batch — see [`max_batch_rows`], which caps
    /// `batch_size` accordingly. Vectors written before this key existed
    /// were truncated at 512 tokens and need a re-embed to include the
    /// tail of each chunk.
    pub max_length: usize,
    /// Rows per forward pass (starting point when `adaptive_batch`).
    /// At most [`max_batch_rows`]`(max_length)`.
    pub batch_size: Option<usize>,
    /// Model pool size. Each model is ~600 MB resident; pool lets parallel
    /// `embed_batch` callers avoid contending on one Mutex. Defaults to 1
    /// (single GPU context is faster than N).
    pub pool_size: Option<usize>,
    /// Adaptive batch-size controller. When true (default), the embedder
    /// hill-climbs `batch_size` in-process against observed throughput
    /// using an EWMA controller. `batch_size` becomes the starting point
    /// rather than a fixed value. Disable to pin batch_size exactly.
    pub adaptive_batch: bool,
    /// Compute device for the embedder. rc.306 P0-7: must be `cuda`.
    /// Present in config for operator visibility and to ensure any
    /// attempt to write a non-CUDA value fails loudly at deserialization.
    pub compute_device: ComputeDevice,
    /// Drop the bge-m3 weights from GPU memory after this many seconds
    /// of no embed requests. `None` (default) = never drop — the model
    /// stays resident forever, matching pre-rc.NNN behavior. Set to a
    /// value like 300 on hosts where another GPU service shares the card
    /// (e.g. `big` running an olmocr VLM alongside distill); first
    /// embed request after a release reloads the model from disk (~10s
    /// warm-up). The release is verified at the ort layer — dropping
    /// `fastembed::TextEmbedding` releases the underlying `ort::Session`
    /// and its CUDA allocations.
    #[serde(default)]
    pub idle_release_secs: Option<u64>,
    /// Free VRAM required before (re)loading the bge-m3 pool, MB. The
    /// pool is ~4.4 GB resident on `big`. Loading under a co-tenant that
    /// has taken the card returns a CUDA OOM that poisons the ort
    /// session, so refuse loudly — and name the holders — instead.
    /// Hosts without an NVIDIA GPU have no signal and skip the gate.
    #[serde(default = "default_vram_floor_mb")]
    pub vram_floor_mb: u64,
}

/// bge-m3 needs ~4.4 GB resident; 5000 MB leaves a little slack for the
/// ort arena without demanding a whole free card.
fn default_vram_floor_mb() -> u64 {
    5000
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            dimension: 1024,
            max_length: 1280,
            batch_size: None,
            pool_size: None,
            adaptive_batch: true,
            compute_device: ComputeDevice::Cuda,
            idle_release_secs: None,
            vram_floor_mb: default_vram_floor_mb(),
        }
    }
}

impl EmbeddingConfig {
    /// Checks that depend only on the embedding settings.
    pub fn validate_self(&self) -> Result<(), DistillError> {
        if self.dimension == 0 {
            return Err(DistillError::Config(
                "embedding.dimension must be > 0".into(),
            ));
        }
        if !(1..=MODEL_MAX_LENGTH).contains(&self.max_length) {
            return Err(DistillError::Config(format!(
                "embedding.max_length must be 1..={MODEL_MAX_LENGTH} (got {})",
                self.max_length
            )));
        }
        let cap = max_batch_rows(self.max_length);
        if let Some(batch) = self.batch_size {
            if batch == 0 {
                return Err(DistillError::Config(
                    "embedding.batch_size must be at least 1".into(),
                ));
            }
            if batch > cap {
                return Err(DistillError::Config(format!(
                    "embedding.batch_size {batch} exceeds {cap}, the most rows that fit the GPU \
                     budget at embedding.max_length {}",
                    self.max_length
                )));
            }
        }
        if self.pool_size == Some(0) {
            return Err(DistillError::Config(
                "embedding.pool_size must be at least 1".into(),
            ));
        }
        if self.idle_release_secs == Some(0) {
            return Err(DistillError::Config(
                "embedding.idle_release_secs must be at least 1 (omit it to never release)".into(),
            ));
        }
        Ok(())
    }

    /// [`Self::validate_self`] plus the relation to the chunker:
    /// `chunk_max_tokens` is the chunker's size, and the embedder must be
    /// able to see all of it.
    pub fn validate(&self, chunk_max_tokens: usize) -> Result<(), DistillError> {
        self.validate_self()?;
        if chunk_max_tokens + CHUNK_HEADER_RESERVE_TOKENS > self.max_length {
            return Err(DistillError::Config(format!(
                "chunk_max_tokens ({chunk_max_tokens}) + {CHUNK_HEADER_RESERVE_TOKENS} header tokens \
                 exceeds embedding.max_length ({}): the tail of every chunk would be \
                 truncated out of its vector. Raise embedding.max_length or lower chunk_max_tokens",
                self.max_length
            )));
        }
        Ok(())
    }

    /// Rows per forward pass to start from: the configured `batch_size`,
    /// else 32, never above the [`max_batch_rows`] cap.
    pub fn initial_batch(&self) -> usize {
        self.batch_size
            .unwrap_or(32)
            .min(max_batch_rows(self.max_length))
    }
}

// ── Client Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillClientConfig {
    /// Distill servers. Empty by default: there is no server to guess at;
    /// commands that need one call [`Self::require_servers`].
    pub servers: Vec<String>,
    pub markdown_dir: PathBuf,
    pub catalog_dir: PathBuf,
    /// Event-watch worker concurrency — how many markdown documents the
    /// local `hs distill watch-events` loop will index in parallel.
    /// `None` means "use the HardwareProfile default for this host"
    /// (Pi=2, AppleSiliconLow=4, AppleSiliconHigh=6, Nvidia*=8, GenericCpu
    /// scales with `cpu_count/4`). Explicit override wins.
    pub concurrency: Option<usize>,
    /// Deadline for one indexing request, seconds. It must leave room
    /// inside the event bus's `ack_wait` (default 7200 s) for the stamp and
    /// publish that follow indexing — otherwise the broker redelivers an
    /// event whose first delivery is still running. See
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
    /// The documented built-in defaults, rooted at `~/home-still`. Loading
    /// ([`DistillClientConfig::load`]) roots them at `home.project_dir` and
    /// never falls back to this value.
    fn default() -> Self {
        Self::with_project_dir(&hs_common::default_project_dir())
    }
}

impl DistillClientConfig {
    /// Defaults with the directories under `project` (`home.project_dir`).
    pub fn with_project_dir(project: &std::path::Path) -> Self {
        Self {
            servers: Vec::new(),
            markdown_dir: project.join("markdown"),
            catalog_dir: project.join("catalog"),
            concurrency: None,
            index_timeout_secs: crate::client::DEFAULT_INDEX_TIMEOUT.as_secs(),
            storage: StorageConfig::default(),
            events: None,
        }
    }

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

    /// The client's effective settings: the documented defaults (rooted at
    /// `home.project_dir`), then the `distill:` section of
    /// `~/.home-still/config.yaml`, then `HS_DISTILL_*` environment
    /// variables. `storage:` and `events:` are read from their own sections.
    /// A missing file is a valid, empty config; a file or section that is
    /// present but malformed is an `Err` naming it.
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::from_file(&ConfigFile::load()?)?)
    }

    /// [`Self::load`] against an already-read config file.
    pub fn from_file(file: &ConfigFile) -> Result<Self, ConfigError> {
        let defaults = Self::with_project_dir(&file.project_dir()?);
        let mut cfg: Self = extract_section(file, CLIENT_SECTION, &defaults)?;
        cfg.storage = file.section("storage")?.unwrap_or_default();
        cfg.events = EventBusConfig::from_file(file)?;
        cfg.validate()
            .map_err(|e| ConfigError::section(file.path(), CLIENT_SECTION, e))?;
        Ok(cfg)
    }

    /// Seconds of `ack_wait` that must remain after an index request ends
    /// (catalog stamp with retries, `distill.completed` publish).
    const ACK_WAIT_HEADROOM_SECS: u64 = 120;

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
        if let Some(events) = self
            .events
            .as_ref()
            .filter(|e| e.backend == hs_common::event_bus::EventsBackend::Nats)
        {
            let ack_wait = events.nats.ack_wait_secs;
            if self.index_timeout_secs + Self::ACK_WAIT_HEADROOM_SECS > ack_wait {
                return Err(DistillError::Config(format!(
                    "distill.index_timeout_secs ({}) + {} s headroom exceeds events.nats.ack_wait_secs \
                     ({ack_wait}): the broker would redeliver an event that is still being indexed",
                    self.index_timeout_secs,
                    Self::ACK_WAIT_HEADROOM_SECS
                )));
            }
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

    #[test]
    fn compute_device_default_is_cuda() {
        let cfg = EmbeddingConfig::default();
        assert!(matches!(cfg.compute_device, ComputeDevice::Cuda));
    }

    #[test]
    fn yaml_with_cuda_loads() {
        let yaml = "compute_device: cuda\n";
        let cfg: EmbeddingConfig = serde_yaml_ng::from_str(yaml).expect("cuda should parse");
        assert!(matches!(cfg.compute_device, ComputeDevice::Cuda));
    }

    #[test]
    fn yaml_with_cpu_fails_loudly() {
        // Distill ships with no CPU path; setting compute_device: cpu
        // must fail deserialization, not silently fall through to Cuda.
        let yaml = "compute_device: cpu\n";
        let err = serde_yaml_ng::from_str::<EmbeddingConfig>(yaml)
            .expect_err("compute_device: cpu must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("variant") || msg.contains("cpu"),
            "error should name the bad variant: {msg}"
        );
    }

    #[test]
    fn yaml_with_unknown_device_fails() {
        let yaml = "compute_device: rocm\n";
        let err = serde_yaml_ng::from_str::<EmbeddingConfig>(yaml)
            .expect_err("unknown compute_device must reject");
        let _ = err.to_string();
    }

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
    fn chunk_larger_than_the_embedder_window_is_rejected() {
        // RA-40: 1000-token chunks against a 512-token window silently lost
        // the back half of every chunk.
        let mut c = cfg();
        c.embedding.max_length = 512;
        let msg = config_error(&c);
        assert!(msg.contains("1000") && msg.contains("512"), "{msg}");
    }

    #[test]
    fn chunk_plus_header_must_fit_the_embedder_window() {
        let mut c = cfg();
        c.embedding.max_length = c.chunk_max_tokens + CHUNK_HEADER_RESERVE_TOKENS;
        c.validate().expect("exactly fitting is allowed");
        c.embedding.max_length -= 1;
        config_error(&c);
    }

    #[test]
    fn embedder_window_cannot_exceed_the_model() {
        let mut c = cfg();
        c.embedding.max_length = MODEL_MAX_LENGTH + 1;
        config_error(&c);
        c.embedding.max_length = 0;
        config_error(&c);
    }

    #[test]
    fn zero_batch_size_is_rejected_at_load() {
        // RA-45d: `step_by(0)` panics inside the embed thread.
        let mut c = cfg();
        c.embedding.batch_size = Some(0);
        assert!(config_error(&c).contains("batch_size"));
    }

    #[test]
    fn batch_size_is_capped_by_the_vram_budget_for_the_window() {
        assert_eq!(max_batch_rows(512), 128, "the historical ceiling");
        assert_eq!(max_batch_rows(1024), 32);
        assert!(max_batch_rows(1280) < max_batch_rows(1024));
        assert_eq!(max_batch_rows(MODEL_MAX_LENGTH), 1, "never below one row");

        let mut c = cfg();
        let cap = max_batch_rows(c.embedding.max_length);
        c.embedding.batch_size = Some(cap);
        c.validate().unwrap();
        c.embedding.batch_size = Some(cap + 1);
        assert!(config_error(&c).contains("batch_size"));
    }

    #[test]
    fn default_initial_batch_respects_the_cap() {
        let mut e = EmbeddingConfig::default();
        assert!(e.initial_batch() <= max_batch_rows(e.max_length));
        assert!(e.initial_batch() >= 1);
        e.max_length = 4096;
        assert_eq!(e.initial_batch(), max_batch_rows(4096));
    }

    #[test]
    fn zero_pool_or_idle_window_is_rejected() {
        let mut c = cfg();
        c.embedding.pool_size = Some(0);
        config_error(&c);
        let mut c = cfg();
        c.embedding.idle_release_secs = Some(0);
        config_error(&c);
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

    #[test]
    fn hnsw_cannot_be_disabled_or_nonsensical() {
        for bad in [
            HnswConfig {
                m: 0,
                ..HnswConfig::default()
            },
            HnswConfig {
                m: 3,
                ..HnswConfig::default()
            },
            HnswConfig {
                ef_construct: 8,
                ..HnswConfig::default()
            },
            HnswConfig {
                max_indexing_threads: 0,
                ..HnswConfig::default()
            },
            HnswConfig {
                search_ef: 0,
                ..HnswConfig::default()
            },
        ] {
            let mut c = cfg();
            c.hnsw = bad.clone();
            config_error(&c);
        }
    }

    #[test]
    fn collection_names_are_restricted() {
        for bad in ["", "../x", "a b", "-lead", "ünï", &"a".repeat(65), "a/b"] {
            assert!(
                validate_collection_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        for good in ["academic_papers", "paper_abstracts", "p-1", "A9"] {
            validate_collection_name(good).unwrap();
        }
    }

    #[test]
    fn configured_collections_must_be_valid_and_unique() {
        let mut c = cfg();
        c.collections = vec!["bad name".into()];
        config_error(&c);

        let mut c = cfg();
        c.collections = vec![c.collection_name.clone()];
        assert!(config_error(&c).contains("twice"));

        let mut c = cfg();
        c.collections = vec!["x".into(), "x".into()];
        config_error(&c);
    }

    #[test]
    fn default_collections_serve_the_workspace_callers() {
        let c = cfg();
        let served: Vec<_> = c.served_collections().collect();
        assert_eq!(
            served,
            ["academic_papers", "paper_abstracts", "personal_docs"]
        );
    }

    // ── Loading (RA-6) ─────────────────────────────────────────────────
    //
    // Through `from_file` against a temporary home directory, with the
    // process environment emptied and then set per test (figment's `Jail`).

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
    fn yaml_collections_replace_the_default_list_and_host_port_are_read() {
        let (_home, file) = home_with(Some(
            "distill_server:\n  host: 127.0.0.1\n  port: 7444\n  collections: [only_this]\n",
        ));
        let loaded = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
        assert_eq!(loaded.collections, ["only_this"]);
        assert_eq!((loaded.host.as_str(), loaded.port), ("127.0.0.1", 7444));
        loaded.validate().unwrap();
        let served: Vec<_> = loaded.served_collections().collect();
        assert_eq!(served, ["academic_papers", "only_this"]);
    }

    #[test]
    fn env_overrides_the_file_and_the_project_dir_moves_the_data_dir() {
        let (_home, file) = home_with(Some(
            "home:\n  project_dir: /srv/hs\ndistill_server:\n  port: 7444\n",
        ));
        let loaded = with_env(&[("HS_DISTILL_PORT", "7555")], || {
            DistillServerConfig::from_file(&file)
        })
        .unwrap();
        assert_eq!(loaded.port, 7555);
        assert_eq!(loaded.qdrant_data_dir, std::path::PathBuf::from("/srv/hs/data/qdrant"));
    }

    #[test]
    fn a_malformed_section_or_env_value_is_an_error_never_the_defaults() {
        for yaml in [
            "distill_server:\n  port: not-a-port\n",
            "distill_server:\n  embedding:\n    compute_device: cpu\n",
            "distill_server: [1, 2]\n",
        ] {
            let (_home, file) = home_with(Some(yaml));
            let err = with_env(&[], || DistillServerConfig::from_file(&file))
                .expect_err(yaml)
                .to_string();
            assert!(err.contains("`distill_server`"), "{yaml}: {err}");
        }
        let (_home, ok) = home_with(None);
        let err = with_env(&[("HS_DISTILL_PORT", "http")], || {
            DistillServerConfig::from_file(&ok)
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("distill_server") && err.to_ascii_lowercase().contains("port"), "{err}");
    }

    #[test]
    fn upsert_sizes_of_zero_are_rejected_not_clamped() {
        let mut c = cfg();
        c.qdrant_upsert_batch = 0;
        config_error(&c);
        let mut c = cfg();
        c.qdrant_upsert_parallelism = 0;
        config_error(&c);
    }

    #[test]
    fn llm_settings_are_checked_only_when_enabled() {
        let mut c = cfg();
        c.ollama_url = "http://localhost:notaport".into();
        c.validate().expect("unused while llm_metadata is false");
        c.llm_metadata = true;
        config_error(&c);
    }

    #[test]
    fn removed_keys_are_reported_not_silently_accepted() {
        let yaml = "distill_server:\n  embedding:\n    model: bge-m3\n    sparse_enabled: true\n    dimension: 1024\n";
        let (_home, file) = home_with(Some(yaml));
        let section = file.section_json(SERVER_SECTION).unwrap().unwrap();
        assert_eq!(
            removed_keys_present(&section),
            ["embedding.model", "embedding.sparse_enabled"]
        );
        // Still loads: the keys were already inert.
        let loaded = with_env(&[], || DistillServerConfig::from_file(&file)).unwrap();
        assert_eq!(loaded.embedding.dimension, 1024);

        let (_home, clean) = home_with(Some("distill_server:\n  port: 7434\n"));
        let section = clean.section_json(SERVER_SECTION).unwrap().unwrap();
        assert!(removed_keys_present(&section).is_empty());
    }

    // ── Client config ──────────────────────────────────────────────────

    fn nats_client_config(index_timeout_secs: u64, ack_wait_secs: u64) -> DistillClientConfig {
        let mut events = EventBusConfig {
            backend: hs_common::event_bus::EventsBackend::Nats,
            nats: hs_common::event_bus::config::NatsYaml::default(),
        };
        events.nats.ack_wait_secs = ack_wait_secs;
        DistillClientConfig {
            index_timeout_secs,
            events: Some(events),
            ..DistillClientConfig::default()
        }
    }

    #[test]
    fn the_client_has_no_default_server_and_no_default_bus() {
        let (_home, file) = home_with(None);
        let cfg = with_env(&[], || DistillClientConfig::from_file(&file)).unwrap();
        assert!(cfg.servers.is_empty());
        assert!(cfg.events.is_none());
        let err = cfg.require_servers().unwrap_err().to_string();
        assert!(err.contains("distill.servers"), "{err}");

        let (_home, file) = home_with(Some(
            "distill:\n  servers: [http://host-a.example:7434]\n  index_timeout_secs: 600\nevents:\n  backend: noop\n",
        ));
        let cfg = with_env(&[], || DistillClientConfig::from_file(&file)).unwrap();
        assert_eq!(cfg.require_servers().unwrap(), ["http://host-a.example:7434"]);
        assert_eq!(cfg.index_timeout_secs, 600);
        assert!(cfg.events.is_some());
    }

    #[test]
    fn a_malformed_client_section_is_an_error_naming_it() {
        for (yaml, section) in [
            ("distill:\n  servers: not-a-list\n", "distill"),
            ("distill:\n  index_timeout_secs: 0\n", "distill"),
            ("storage:\n  backend: carrier-pigeon\n", "storage"),
            ("events:\n  backend: carrier-pigeon\n", "events"),
            ("events:\n  backend: nats\n  nats:\n    ack_wait_secs: 100\n", "distill"),
        ] {
            let (_home, file) = home_with(Some(yaml));
            let err = with_env(&[], || DistillClientConfig::from_file(&file))
                .expect_err(yaml)
                .to_string();
            assert!(err.contains(&format!("`{section}`")), "{yaml}: {err}");
        }
    }

    #[test]
    fn default_client_config_is_valid_against_the_default_ack_wait() {
        let mut c = DistillClientConfig::default();
        c.validate().unwrap();
        c.events = Some(hs_common::event_bus::EventBusConfig {
            backend: hs_common::event_bus::EventsBackend::Nats,
            nats: hs_common::event_bus::config::NatsYaml::default(),
        });
        c.validate()
            .expect("1800 s index timeout fits the 7200 s ack_wait");
        assert_eq!(c.index_timeout(), crate::client::DEFAULT_INDEX_TIMEOUT);
    }

    #[test]
    fn index_timeout_must_fit_inside_ack_wait() {
        // Equal to ack_wait would let the broker redeliver mid-index.
        assert!(nats_client_config(7200, 7200).validate().is_err());
        assert!(nats_client_config(7100, 7200).validate().is_err());
        nats_client_config(7080, 7200).validate().unwrap();
    }

    #[test]
    fn zero_timeout_or_concurrency_is_rejected() {
        assert!(nats_client_config(0, 7200).validate().is_err());
        let c = DistillClientConfig {
            concurrency: Some(0),
            ..DistillClientConfig::default()
        };
        assert!(c.validate().is_err());
    }
}
