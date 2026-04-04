use std::path::PathBuf;

use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};

// ── Server Config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillServerConfig {
    pub host: String,
    pub port: u16,
    pub qdrant_url: String,
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
        Self {
            host: "0.0.0.0".into(),
            port: 7434,
            qdrant_url: "http://localhost:6334".into(),
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
}

impl Default for DistillClientConfig {
    fn default() -> Self {
        let project = hs_common::resolve_project_dir();
        Self {
            servers: vec!["http://localhost:7434".into()],
            markdown_dir: project.join("markdown"),
            catalog_dir: project.join("catalog"),
        }
    }
}

impl DistillClientConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let home = dirs::home_dir().unwrap_or_default();
        let config_path = home.join(hs_common::CONFIG_REL_PATH);

        Figment::from(Serialized::defaults(Self::default()))
            .merge(Yaml::file(&config_path).nested())
            .merge(Env::prefixed("HS_DISTILL_"))
            .select("distill")
            .extract()
            .map_err(Box::new)
    }
}
