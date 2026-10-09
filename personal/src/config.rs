use crate::error::{PersonalError, Result};
use crate::models::Category;
use figment::{
    providers::{Env, Serialized},
    Figment,
};
use hs_common::config_file::{env_key_path, unknown_env_names, ConfigError, ConfigFile};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Prefix of the environment variables that override config keys.
const ENV_PREFIX: &str = "HOME_STILL_";

/// The system-wide config file, merged under the user's.
const SYSTEM_CONFIG_PATH: &str = "/etc/home-still/config.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
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
    /// The categories this store accepts: a subset of [`Category::ALL`],
    /// validated at load. The naming prompt offers exactly this list and an
    /// ingest whose category is outside it fails — no runtime extensions, no
    /// fallbacks to `other` when the LLM hallucinates.
    pub categories: Vec<String>,
    /// `home.project_dir`, which `storage_dir` is relative to. Not a
    /// `personal:` key: set by [`Config::load`] from the `home:` section.
    #[serde(skip)]
    pub project_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingConfig {
    pub ollama_url: String,
    pub model: String,
    pub max_input_tokens: usize,
}

impl Default for Config {
    /// The documented built-in defaults, rooted at `~/home-still`. Loading
    /// ([`Config::load`]) roots them at `home.project_dir` and never falls
    /// back to this value.
    fn default() -> Self {
        Self {
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
            project_dir: hs_common::default_project_dir(),
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

fn config_error(e: impl std::fmt::Display) -> PersonalError {
    PersonalError::Config(e.to_string())
}

/// The key tree environment variables are matched against: the `personal`
/// section's defaults, and nothing else.
fn known_keys() -> Result<serde_json::Value> {
    let mut root = serde_json::Map::new();
    root.insert(
        "personal".into(),
        serde_json::to_value(Config::default()).map_err(config_error)?,
    );
    Ok(serde_json::Value::Object(root))
}

impl Config {
    /// Load the effective configuration: the system file, the user file
    /// (`~/.home-still/config.yaml`), then `HOME_STILL_PERSONAL_*`
    /// environment overrides, validated.
    ///
    /// Environment keys name the config path with `_` between *words as well
    /// as levels*: `HOME_STILL_PERSONAL_STORAGE_DIR` is `personal.storage_dir`,
    /// `HOME_STILL_PERSONAL_NAMING_MAX_INPUT_TOKENS` is
    /// `personal.naming.max_input_tokens`. A blanket `.split("_")` turns
    /// `storage_dir` into `storage.dir` and the override silently does
    /// nothing, so each variable is matched against the known key tree
    /// instead. A `HOME_STILL_PERSONAL_*` variable that names no key is an
    /// error.
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or(ConfigError::NoHomeDir)
            .map_err(config_error)?;
        let user = ConfigFile::load_in(&home).map_err(config_error)?;
        let system =
            ConfigFile::load_at(Path::new(SYSTEM_CONFIG_PATH), &home).map_err(config_error)?;
        Self::load_from(&system, &user)
    }

    /// [`Self::load`] against already-read config files.
    pub fn load_from(system: &ConfigFile, user: &ConfigFile) -> Result<Self> {
        let project_dir = user.project_dir().map_err(config_error)?;
        let keys = known_keys()?;
        let mut figment = Figment::new().merge(Serialized::default("personal", Self::default()));
        for file in [system, user] {
            if let Some(section) = file.section_json("personal").map_err(config_error)? {
                // A stale or misspelt `personal:` key is ignored; say so.
                hs_common::config_file::warn_unknown_keys(
                    file.path(),
                    "personal",
                    &section,
                    &keys["personal"],
                    &[],
                );
                figment = figment.merge(Serialized::default("personal", section));
            }
        }

        let unknown = unknown_env_names(
            ENV_PREFIX,
            &format!("{ENV_PREFIX}PERSONAL_"),
            &keys,
            std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
        );
        if !unknown.is_empty() {
            return Err(PersonalError::Config(format!(
                "environment variable(s) {} name no personal config key (words in a key are \
                 joined by `_`, e.g. HOME_STILL_PERSONAL_STORAGE_DIR); fix or unset them",
                unknown.join(", ")
            )));
        }
        figment = figment.merge(
            Env::prefixed(ENV_PREFIX)
                .filter_map(move |key| env_key_path(&keys, key.as_str()).map(Into::into)),
        );

        let mut cfg: Config = figment.focus("personal").extract().map_err(|e| {
            PersonalError::Config(format!(
                "{}: invalid `personal` section: {e}",
                user.path().display()
            ))
        })?;
        // A `personal:` key cannot move the project directory.
        cfg.project_dir = project_dir;

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
        // A configured name the application has no category for could never
        // match an ingest, so it is a typo to report, not to carry.
        for name in &cfg.categories {
            if name.parse::<Category>().is_err() {
                return Err(PersonalError::Config(format!(
                    "personal.categories entry {name:?} is not a known category (known: {})",
                    Category::ALL
                        .iter()
                        .map(Category::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        Ok(cfg)
    }

    /// Filesystem root for personal documents: `storage_dir` under the
    /// project directory the config was loaded with.
    pub fn root_dir(&self) -> PathBuf {
        self.project_dir.join(&self.storage_dir)
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

    /// A category a caller named (`--category`, the MCP `category` argument)
    /// as one of this store's: parsed case-insensitively and required to be
    /// in `personal.categories`. A name outside it matches no document, so it
    /// is an error, not an empty result.
    pub fn resolve_category(&self, name: &str) -> Result<Category> {
        let category: Category = name.trim().parse()?;
        if self
            .categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category.as_str()))
        {
            Ok(category)
        } else {
            Err(PersonalError::UnknownCategory(name.trim().to_string()))
        }
    }
}
