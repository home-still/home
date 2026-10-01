//! The one reader of `~/.home-still/config.yaml` for the settings every
//! binary shares (`home:`, `storage:`, `events:`, `logs:`).
//!
//! Rules, identical for every caller:
//!
//! * The file being **absent** is not an error: every section is then
//!   absent and the documented default of each key applies.
//! * A section being **absent** (or an empty `key:`) is `Ok(None)`; the
//!   caller applies the section's documented default, or reports that the
//!   section is required.
//! * A file or section that is **present but malformed** is an `Err` naming
//!   the file and the section. Nothing in this module substitutes a default
//!   for something that failed to load.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};

use crate::{CONFIG_REL_PATH, PROJECT_DIR_DEFAULT};

/// Why the config file (or one of its sections) could not be read.
#[derive(Debug)]
pub enum ConfigError {
    /// `$HOME` is not set, so the config file cannot be located.
    NoHomeDir,
    /// The file exists but could not be read.
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not valid YAML, or its top level is not a mapping.
    Malformed { path: PathBuf, reason: String },
    /// One section (or key) is present but invalid.
    Section {
        path: PathBuf,
        section: String,
        reason: String,
    },
}

impl ConfigError {
    /// An invalid section, for callers that find a problem after
    /// deserializing it (a value out of range, a combination that cannot
    /// work).
    pub fn section(path: &Path, section: &str, reason: impl fmt::Display) -> Self {
        Self::Section {
            path: path.to_path_buf(),
            section: section.to_string(),
            reason: reason.to_string(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHomeDir => write!(
                f,
                "cannot locate {CONFIG_REL_PATH}: the home directory is unknown (is $HOME set?)"
            ),
            Self::Unreadable { path, source } => {
                write!(f, "{}: cannot read the config file: {source}", path.display())
            }
            Self::Malformed { path, reason } => {
                write!(f, "{}: not a valid config file: {reason}", path.display())
            }
            Self::Section {
                path,
                section,
                reason,
            } => write!(f, "{}: invalid `{section}` section: {reason}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The parsed `~/.home-still/config.yaml` (or an empty document when the
/// file does not exist).
#[derive(Debug, Clone)]
pub struct ConfigFile {
    home: PathBuf,
    path: PathBuf,
    exists: bool,
    root: Mapping,
}

/// The `home:` section. Unknown keys are tolerated (other tools own them);
/// the two keys below must be strings when present.
#[derive(Debug, Default, Deserialize)]
struct HomeYaml {
    project_dir: Option<String>,
    log_dir: Option<String>,
}

impl ConfigFile {
    /// Read `$HOME/.home-still/config.yaml`.
    pub fn load() -> Result<Self, ConfigError> {
        let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir)?;
        Self::load_in(&home)
    }

    /// Read `<home>/.home-still/config.yaml`. `home` is also what `~/` in
    /// path values expands to and what the default project directory hangs
    /// off, so a test can pass a temporary directory instead of touching
    /// `$HOME`.
    pub fn load_in(home: &Path) -> Result<Self, ConfigError> {
        Self::load_at(&home.join(CONFIG_REL_PATH), home)
    }

    /// Read the config file at `path` (the system-wide
    /// `/etc/home-still/config.yaml`, say). Same rules as [`Self::load_in`];
    /// `home` is only used for `~/` expansion and the default project
    /// directory.
    pub fn load_at(path: &Path, home: &Path) -> Result<Self, ConfigError> {
        let path = path.to_path_buf();
        let (exists, root) = match std::fs::read_to_string(&path) {
            Ok(text) => (true, parse_root(&path, &text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, Mapping::new()),
            Err(source) => {
                return Err(ConfigError::Unreadable {
                    path,
                    source,
                })
            }
        };
        Ok(Self {
            home: home.to_path_buf(),
            path,
            exists,
            root,
        })
    }

    /// Where the file is (or would be).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file exists. An absent file is a valid, empty config.
    pub fn exists(&self) -> bool {
        self.exists
    }

    /// The top-level section `name` as a JSON object, for loaders that layer
    /// it over defaults and environment overrides. Same absent/malformed
    /// rules as [`Self::section`]; additionally the section must be a
    /// mapping (a list or a scalar would be silently overridden by, or
    /// ignored in favour of, the defaults it is layered over).
    pub fn section_json(&self, name: &str) -> Result<Option<serde_json::Value>, ConfigError> {
        let section = self.section::<serde_json::Value>(name)?;
        match section {
            Some(value) if !value.is_object() => Err(ConfigError::section(
                &self.path,
                name,
                "must be a mapping of keys",
            )),
            other => Ok(other),
        }
    }

    /// Deserialize the top-level section `name`. `Ok(None)` when the file
    /// or the section is absent (or the key has no value); `Err` naming the
    /// section when it is present but does not fit `T`.
    pub fn section<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, ConfigError> {
        match self.root.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_yaml_ng::from_value(value.clone())
                .map(Some)
                .map_err(|e| ConfigError::section(&self.path, name, e)),
        }
    }

    /// The project directory: `home.project_dir` (with `~/` expanded) or the
    /// documented default `~/home-still`.
    pub fn project_dir(&self) -> Result<PathBuf, ConfigError> {
        match self.home_section()?.project_dir {
            Some(value) => self.expand("home.project_dir", &value),
            None => Ok(self.home.join(PROJECT_DIR_DEFAULT)),
        }
    }

    /// The log directory: `home.log_dir` (with `~/` expanded) or the
    /// documented default `<project_dir>/logs`.
    pub fn log_dir(&self) -> Result<PathBuf, ConfigError> {
        match self.home_section()?.log_dir {
            Some(value) => self.expand("home.log_dir", &value),
            None => Ok(self.project_dir()?.join("logs")),
        }
    }

    fn home_section(&self) -> Result<HomeYaml, ConfigError> {
        Ok(self.section::<HomeYaml>("home")?.unwrap_or_default())
    }

    fn expand(&self, key: &str, value: &str) -> Result<PathBuf, ConfigError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ConfigError::section(
                &self.path,
                "home",
                format!("`{key}` is empty (omit it to use the default)"),
            ));
        }
        if value == "~" {
            return Ok(self.home.clone());
        }
        Ok(match value.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(value),
        })
    }
}

fn parse_root(path: &Path, text: &str) -> Result<Mapping, ConfigError> {
    match serde_yaml_ng::from_str::<Value>(text) {
        // An empty (or comment-only) file parses as null: no sections.
        Ok(Value::Null) => Ok(Mapping::new()),
        Ok(Value::Mapping(root)) => Ok(root),
        Ok(other) => Err(ConfigError::Malformed {
            path: path.to_path_buf(),
            reason: format!(
                "the top level must be a mapping of sections, found {}",
                kind_of(&other)
            ),
        }),
        Err(e) => Err(ConfigError::Malformed {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

/// The built-in project directory, `~/home-still`, ignoring the config file.
/// This is what `Default` impls use; loaders resolve the effective
/// directory with [`resolve_project_dir`] instead.
pub fn default_project_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(PROJECT_DIR_DEFAULT)
}

/// The effective project directory (`home.project_dir`, else `~/home-still`).
/// An unreadable or malformed config file is an error, never a silent
/// `~/home-still`.
pub fn resolve_project_dir() -> Result<PathBuf, ConfigError> {
    ConfigFile::load()?.project_dir()
}

/// The effective log directory (`home.log_dir`, else `<project_dir>/logs`).
pub fn resolve_log_dir() -> Result<PathBuf, ConfigError> {
    ConfigFile::load()?.log_dir()
}

/// Resolve an environment variable name (without its prefix, e.g.
/// `PAPER_DOWNLOAD_TIMEOUT_SECS`) to the dotted config path it overrides
/// (`paper.download.timeout_secs`), by finding the way to group the
/// `_`-separated words into keys that exists in `tree`. `None` when no
/// grouping names a leaf.
///
/// A blanket `Env::split("_")` cannot express keys that contain `_`
/// (`timeout_secs` becomes `timeout.secs`), which silently made documented
/// overrides do nothing; matching against the known key tree is exact.
pub fn env_key_path(tree: &serde_json::Value, name: &str) -> Option<String> {
    fn walk(node: &serde_json::Value, words: &[&str], path: &mut Vec<String>) -> bool {
        let Some(object) = node.as_object() else {
            return words.is_empty();
        };
        if words.is_empty() {
            return false;
        }
        for take in 1..=words.len() {
            let key = words[..take].join("_");
            if let Some(child) = object.get(&key) {
                path.push(key);
                if walk(child, &words[take..], path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }

    let lowered = name.to_ascii_lowercase();
    let words: Vec<&str> = lowered.split('_').collect();
    let mut path = Vec::new();
    walk(tree, &words, &mut path).then(|| path.join("."))
}

/// Names of environment variables that start with `section_prefix`
/// (case-insensitively, e.g. `HOME_STILL_PAPER_`) but match no key in
/// `tree`. Such a variable is a typo that would otherwise be ignored
/// without a word.
pub fn unknown_env_names(
    env_prefix: &str,
    section_prefix: &str,
    tree: &serde_json::Value,
    names: impl Iterator<Item = String>,
) -> Vec<String> {
    let section_prefix = section_prefix.to_ascii_uppercase();
    let mut unknown: Vec<String> = names
        .filter(|name| name.to_ascii_uppercase().starts_with(&section_prefix))
        .filter(|name| {
            name.get(env_prefix.len()..)
                .is_some_and(|key| env_key_path(tree, key).is_none())
        })
        .collect();
    unknown.sort();
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(config: Option<&str>) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if let Some(text) = config {
            let path = home.path().join(CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        home
    }

    #[test]
    fn an_absent_file_gives_the_documented_defaults() {
        let home = home_with(None);
        let file = ConfigFile::load_in(home.path()).unwrap();
        assert!(!file.exists());
        assert_eq!(file.project_dir().unwrap(), home.path().join("home-still"));
        assert_eq!(
            file.log_dir().unwrap(),
            home.path().join("home-still").join("logs")
        );
        assert!(file.section::<serde_yaml_ng::Value>("storage").unwrap().is_none());
    }

    #[test]
    fn an_absent_key_gives_the_default_and_a_present_key_is_honoured() {
        let home = home_with(Some("home:\n  log_dir: \"~/elsewhere\"\nstorage: {}\n"));
        let file = ConfigFile::load_in(home.path()).unwrap();
        assert_eq!(file.project_dir().unwrap(), home.path().join("home-still"));
        assert_eq!(file.log_dir().unwrap(), home.path().join("elsewhere"));

        let home = home_with(Some(
            "# comment\nhome:\n  project_dir: '/mnt/project'   # inline comment\n  log_dir: /var/log/hs\n",
        ));
        let file = ConfigFile::load_in(home.path()).unwrap();
        assert_eq!(file.project_dir().unwrap(), PathBuf::from("/mnt/project"));
        assert_eq!(file.log_dir().unwrap(), PathBuf::from("/var/log/hs"));
    }

    #[test]
    fn project_dir_is_found_however_the_yaml_is_laid_out() {
        // The hand-rolled scanner keyed on indentation and a `home:` line;
        // a flow-style mapping and a tilde-only value defeated it.
        let home = home_with(Some("home: {project_dir: ~/flow}\n"));
        let file = ConfigFile::load_in(home.path()).unwrap();
        assert_eq!(file.project_dir().unwrap(), home.path().join("flow"));

        // A `project_dir:` under any other section is not the project dir.
        let home = home_with(Some("other:\n  project_dir: /wrong\n"));
        let file = ConfigFile::load_in(home.path()).unwrap();
        assert_eq!(file.project_dir().unwrap(), home.path().join("home-still"));
    }

    #[test]
    fn a_malformed_file_is_an_error_naming_the_file() {
        let home = home_with(Some("home:\n  project_dir: [unclosed\n"));
        let err = ConfigFile::load_in(home.path()).unwrap_err().to_string();
        assert!(err.contains("config.yaml"), "{err}");

        let home = home_with(Some("- just\n- a list\n"));
        let err = ConfigFile::load_in(home.path()).unwrap_err().to_string();
        assert!(err.contains("mapping"), "{err}");
    }

    #[test]
    fn a_malformed_home_key_is_an_error_not_the_default() {
        for yaml in [
            "home:\n  project_dir: [a, b]\n",
            "home:\n  project_dir: {nested: x}\n",
            "home: just-a-string\n",
            "home:\n  project_dir: \"\"\n",
        ] {
            let home = home_with(Some(yaml));
            let file = ConfigFile::load_in(home.path()).unwrap();
            let err = file.project_dir().unwrap_err().to_string();
            assert!(err.contains("home"), "{yaml}: {err}");
            assert!(file.log_dir().is_err(), "{yaml}: log_dir must not hide it");
        }
    }

    #[test]
    fn a_malformed_section_names_the_section() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Storage {
            backend: String,
        }
        let home = home_with(Some("storage:\n  backend: [not, a, string]\n"));
        let file = ConfigFile::load_in(home.path()).unwrap();
        let err = file.section::<Storage>("storage").unwrap_err().to_string();
        assert!(err.contains("`storage`"), "{err}");
        // A section that is simply not there is not an error.
        assert!(file.section::<Storage>("events").unwrap().is_none());
    }

    #[test]
    fn env_names_group_words_into_keys_and_unknown_names_are_listed() {
        let tree = serde_json::json!({
            "paper": { "download_path": "x", "download": { "timeout_secs": 1 } },
            "storage": { "s3": { "access_key": "" } },
        });
        assert_eq!(
            env_key_path(&tree, "PAPER_DOWNLOAD_PATH").as_deref(),
            Some("paper.download_path")
        );
        assert_eq!(
            env_key_path(&tree, "PAPER_DOWNLOAD_TIMEOUT_SECS").as_deref(),
            Some("paper.download.timeout_secs")
        );
        assert_eq!(
            env_key_path(&tree, "STORAGE_S3_ACCESS_KEY").as_deref(),
            Some("storage.s3.access_key")
        );
        assert_eq!(env_key_path(&tree, "PAPER_DOWNLOAD_TIMEOUT"), None);

        let names = [
            "HOME_STILL_PAPER_DOWNLOAD_PATH",
            "HOME_STILL_PAPER_DOWNLOAD_TIMEOUT",
            "HOME_STILL_STORAGE_BACKEND",
            "UNRELATED",
        ]
        .map(String::from);
        assert_eq!(
            unknown_env_names("HOME_STILL_", "HOME_STILL_PAPER_", &tree, names.into_iter()),
            vec!["HOME_STILL_PAPER_DOWNLOAD_TIMEOUT".to_string()]
        );
    }
}
