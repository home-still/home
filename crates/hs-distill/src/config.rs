use std::path::PathBuf;
use std::sync::Arc;

use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::hardware_profile::HardwareProfile;
use hs_common::storage::{Storage, StorageConfig};
use serde::{Deserialize, Serialize};

// ── Server Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillServerConfig {
    pub host: String,
    pub port: u16,
    pub qdrant_url: String,
    pub qdrant_data_dir: PathBuf,
    pub collection_name: String,
    pub embedding: EmbeddingConfig,
    pub chunk_max_tokens: usize,
    pub chunk_overlap: usize,
    /// Number of chunks per Qdrant upsert request. Each chunk carries a
    /// 1024-dim f32 dense vector plus sparse + payload (~3–5 KB), so the
    /// default 1000 sits well within Qdrant's 4 MB gRPC frame limit
    /// while amortizing per-request overhead.
    pub qdrant_upsert_batch: usize,
    /// How many Qdrant upsert requests to fire in parallel per document.
    /// Qdrant handles many concurrent writes to one collection cheaply,
    /// so this keeps the upsert phase from being the slow link after a
    /// fast embed.
    pub qdrant_upsert_parallelism: usize,
    pub llm_metadata: bool,
    pub metadata_model: String,
    pub ollama_url: String,
}

impl Default for DistillServerConfig {
    fn default() -> Self {
        let project = hs_common::resolve_project_dir();
        Self {
            host: "0.0.0.0".into(),
            port: 7434,
            qdrant_url: "http://localhost:6334".into(),
            qdrant_data_dir: project.join("data").join("qdrant"),
            collection_name: "academic_papers".into(),
            embedding: EmbeddingConfig::default(),
            chunk_max_tokens: 1000,
            chunk_overlap: 100,
            qdrant_upsert_batch: 1000,
            qdrant_upsert_parallelism: 4,
            llm_metadata: false,
            metadata_model: "llama3.2:latest".into(),
            ollama_url: "http://localhost:11434".into(),
        }
    }
}

impl DistillServerConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let home = dirs::home_dir().unwrap_or_default();
        let config_path = home.join(hs_common::CONFIG_REL_PATH);

        Figment::from(Serialized::defaults(Self::default()))
            .merge(Yaml::file(&config_path).nested())
            .merge(Env::prefixed("HS_DISTILL_"))
            .select("distill_server")
            .extract()
            .map_err(Box::new)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub model: String,
    pub dimension: usize,
    pub batch_size: Option<usize>,
    /// Model pool size. Each model is ~600 MB resident; pool lets parallel
    /// `embed_batch` callers avoid contending on one Mutex. `None` means
    /// "pick per device": CUDA=1 (single GPU context is faster than N),
    /// CPU= min(HardwareProfile::distill_concurrency, 4).
    pub pool_size: Option<usize>,
    /// Adaptive batch-size controller. When true (default), the embedder
    /// hill-climbs `batch_size` in-process against observed throughput
    /// using an EWMA controller. `batch_size` becomes the starting point
    /// rather than a fixed value. Disable to pin batch_size exactly.
    pub adaptive_batch: bool,
    pub sparse_enabled: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "bge-m3".into(),
            dimension: 1024,
            batch_size: None,
            pool_size: None,
            adaptive_batch: true,
            sparse_enabled: true,
        }
    }
}

// ── Client Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillClientConfig {
    pub servers: Vec<String>,
    pub markdown_dir: PathBuf,
    pub catalog_dir: PathBuf,
    /// Event-watch worker concurrency — how many markdown documents the
    /// local `hs distill watch-events` loop will index in parallel.
    /// `None` means "use the HardwareProfile default for this host"
    /// (Pi=2, AppleSiliconLow=4, AppleSiliconHigh=6, Nvidia*=8, GenericCpu
    /// scales with `cpu_count/4`). Explicit override wins.
    pub concurrency: Option<usize>,
    #[serde(skip)]
    pub storage: StorageConfig,
    #[serde(skip)]
    pub events: EventBusConfig,
}

impl Default for DistillClientConfig {
    fn default() -> Self {
        let project = hs_common::resolve_project_dir();
        Self {
            servers: vec!["http://localhost:7434".into()],
            markdown_dir: project.join("markdown"),
            catalog_dir: project.join("catalog"),
            concurrency: None,
            storage: StorageConfig::default(),
            events: EventBusConfig::default(),
        }
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
}

impl DistillClientConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let home = dirs::home_dir().unwrap_or_default();
        let config_path = home.join(hs_common::CONFIG_REL_PATH);

        let figment = Figment::from(Serialized::defaults(Self::default()))
            .merge(Yaml::file(&config_path).nested())
            .merge(Env::prefixed("HS_DISTILL_"));

        let storage = figment
            .clone()
            .select("storage")
            .extract::<StorageConfig>()
            .unwrap_or_default();

        let events = figment
            .clone()
            .select("events")
            .extract::<EventBusConfig>()
            .unwrap_or_default();

        let mut cfg: DistillClientConfig = figment.select("distill").extract().map_err(Box::new)?;
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
