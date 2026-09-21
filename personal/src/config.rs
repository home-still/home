use crate::error::{PersonalError, Result};
use crate::models::Category;
use figment::{
    providers::{Env, Format, Yaml},
    Figment,
};
use hs_common::CONFIG_REL_PATH;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub collection_name: String,
    pub storage_dir: String,
    /// Subdirectory under `storage_dir` where files are staged for MCP-driven
    /// ingestion. The MCP `personal_add` tool accepts a *filename* and resolves
    /// it under this path; absolute paths and traversal attempts are rejected.
    /// This is the single place an external agent can introduce bytes to the
    /// personal store — drop the file here yourself, then ask the agent to
    /// ingest it by name.
    pub ingest_inbox: String,
    pub naming: NamingConfig,
    pub distill_url: String,
    pub scribe_url: String,
    /// The fixed category taxonomy. Loaded from config so the user can rename,
    /// but the application enforces that the LLM's pick is in this list — no
    /// runtime extensions, no fallbacks to `other` when the LLM hallucinates.
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingConfig {
    pub ollama_url: String,
    pub model: String,
    pub max_input_tokens: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            collection_name: "personal_docs".to_string(),
            storage_dir: "personal".to_string(),
            ingest_inbox: "inbox".to_string(),
            naming: NamingConfig::default(),
            distill_url: "http://localhost:7434".to_string(),
            scribe_url: "http://localhost:7433".to_string(),
            categories: Category::ALL
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        }
    }
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            ollama_url: "http://localhost:11434".to_string(),
            // Default to a small text model that's commonly already pulled on
            // the workstation; user can override via
            // ~/.home-still/config.yaml `personal.naming.model`.
            model: "qwen2.5:7b".to_string(),
            max_input_tokens: 2048,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
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

        let cfg: Config = figment
            .focus("personal")
            .extract()
            .map_err(|e| PersonalError::Config(format!("failed to parse personal config: {e}")))?;

        if cfg.collection_name.trim().is_empty() {
            return Err(PersonalError::Config(
                "personal.collection_name must be set; refusing to fall back".into(),
            ));
        }
        if cfg.categories.is_empty() {
            return Err(PersonalError::Config(
                "personal.categories must list at least one category".into(),
            ));
        }

        Ok(cfg)
    }

    /// Filesystem root for personal documents. Mirrors `hs_common::resolve_personal_dir`
    /// but resolved relative to the loaded config so `storage_dir` overrides take effect.
    pub fn root_dir(&self) -> PathBuf {
        hs_common::resolve_project_dir().join(&self.storage_dir)
    }

    pub fn markdown_dir(&self) -> PathBuf {
        self.root_dir().join("markdown")
    }

    /// Directory the MCP `personal_add` tool resolves filenames against.
    /// Relative `ingest_inbox` values are joined under `root_dir`; absolute
    /// values are taken as-is so a user can point the inbox at, e.g., an NFS
    /// drop folder shared with a phone scanner.
    pub fn inbox_dir(&self) -> PathBuf {
        let p = PathBuf::from(&self.ingest_inbox);
        if p.is_absolute() {
            p
        } else {
            self.root_dir().join(p)
        }
    }
}
