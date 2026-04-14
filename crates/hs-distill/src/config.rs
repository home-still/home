use std::path::PathBuf;
use std::sync::Arc;

use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use hs_common::event_bus::{EventBus, EventBusConfig};
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
    pub sparse_enabled: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "bge-m3".into(),
            dimension: 1024,
            batch_size: None,
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
            storage: StorageConfig::default(),
            events: EventBusConfig::default(),
        }
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
