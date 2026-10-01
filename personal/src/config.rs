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
        let mut defaults = Self::default();
        defaults.project_dir = project_dir.clone();

        let mut figment = Figment::new().merge(Serialized::default("personal", &defaults));
        for file in [system, user] {
            if let Some(section) = file.section_json("personal").map_err(config_error)? {
                figment = figment.merge(Serialized::default("personal", section));
            }
        }

        let keys = known_keys()?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load against a config file with `yaml` as its content (or none) under
    /// a hermetic environment plus `vars`.
    fn load_yaml(yaml: Option<&str>, vars: &[(&str, &str)]) -> Result<Config> {
        let mut out = None;
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            for (k, v) in vars {
                jail.set_env(k, v);
            }
            let home = jail.directory().to_path_buf();
            if let Some(yaml) = yaml {
                let path = home.join(hs_common::CONFIG_REL_PATH);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, yaml).unwrap();
            }
            let user = ConfigFile::load_in(&home).unwrap();
            let system = ConfigFile::load_at(Path::new("/nonexistent/config.yaml"), &home).unwrap();
            out = Some(Config::load_from(&system, &user));
            Ok(())
        });
        out.expect("closure ran")
    }

    #[test]
    fn documented_multi_word_env_keys_bind() {
        // `.split("_")` read `STORAGE_DIR` as `storage.dir`, a key that does
        // not exist, so every one of these overrides was dropped silently.
        let c = load_yaml(
            None,
            &[
                ("HOME_STILL_PERSONAL_STORAGE_DIR", "scans"),
                ("HOME_STILL_PERSONAL_COLLECTION_NAME", "my_docs"),
                ("HOME_STILL_PERSONAL_NAMING_MAX_INPUT_TOKENS", "512"),
                (
                    "HOME_STILL_PERSONAL_NAMING_OLLAMA_URL",
                    "http://llm.example:11434",
                ),
                ("HOME_STILL_PERSONAL_INGEST_INBOX", "drop"),
            ],
        )
        .unwrap();
        assert_eq!(c.storage_dir, "scans");
        assert_eq!(c.collection_name, "my_docs");
        assert_eq!(c.naming.max_input_tokens, 512);
        assert_eq!(c.naming.ollama_url, "http://llm.example:11434");
        assert_eq!(c.ingest_inbox, "drop");
        assert_eq!(c.naming.model, "qwen2.5:7b", "others keep their defaults");
    }

    #[test]
    fn env_beats_the_file_and_the_file_beats_the_default() {
        let yaml = "personal:\n  storage_dir: from_file\n  naming:\n    model: file-model\n";
        let c = load_yaml(Some(yaml), &[]).unwrap();
        assert_eq!(
            (c.storage_dir.as_str(), c.naming.model.as_str()),
            ("from_file", "file-model")
        );
        let c = load_yaml(
            Some(yaml),
            &[("HOME_STILL_PERSONAL_STORAGE_DIR", "from_env")],
        )
        .unwrap();
        assert_eq!(c.storage_dir, "from_env");
        assert_eq!(c.naming.model, "file-model");
    }

    #[test]
    fn an_env_key_that_names_nothing_is_an_error_not_silence() {
        let err = load_yaml(None, &[("HOME_STILL_PERSONAL_STORAGE", "x")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("HOME_STILL_PERSONAL_STORAGE"), "{err}");
        // Variables for the other tools' sections are theirs, not ours.
        load_yaml(None, &[("HOME_STILL_PAPER_DOWNLOAD_PATH", "/x")]).unwrap();
    }

    #[test]
    fn a_malformed_section_is_an_error_not_the_defaults() {
        for yaml in [
            "personal:\n  enabled: maybe\n",
            "personal:\n  categories: not-a-list\n",
            "personal: [1, 2]\n",
            "home:\n  project_dir: [a]\n",
        ] {
            assert!(load_yaml(Some(yaml), &[]).is_err(), "{yaml}");
        }
        let err = load_yaml(Some("personal:\n  enabled: maybe\n"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("personal"), "{err}");
    }

    #[test]
    fn the_store_lives_under_home_project_dir() {
        let c = load_yaml(Some("home:\n  project_dir: /srv/hs\n"), &[]).unwrap();
        assert_eq!(c.root_dir(), PathBuf::from("/srv/hs/personal"));
        assert_eq!(c.markdown_dir(), PathBuf::from("/srv/hs/personal/markdown"));
        assert_eq!(c.inbox_dir(), PathBuf::from("/srv/hs/personal/inbox"));
    }
}
