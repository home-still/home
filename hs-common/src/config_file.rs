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
                write!(
                    f,
                    "{}: cannot read the config file: {source}",
                    path.display()
                )
            }
            Self::Malformed { path, reason } => {
                write!(f, "{}: not a valid config file: {reason}", path.display())
            }
            Self::Section {
                path,
                section,
                reason,
            } => write!(
                f,
                "{}: invalid `{section}` section: {reason}",
                path.display()
            ),
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
#[derive(Clone)]
pub struct ConfigFile {
    home: PathBuf,
    path: PathBuf,
    exists: bool,
    root: Mapping,
}

/// Section and key names only: the parsed document can hold secrets (S3
/// keys, tokens), so no value is ever printed.
impl fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sections: Vec<String> = self
            .root
            .keys()
            .map(|k| k.as_str().unwrap_or("<non-string key>").to_string())
            .collect();
        f.debug_struct("ConfigFile")
            .field("path", &self.path)
            .field("exists", &self.exists)
            .field("sections", &sections)
            .finish_non_exhaustive()
    }
}

/// The `home:` section. These two keys decide where all data and logs live,
/// so an unknown key (a typo such as `project_directory`) is an error naming
/// the valid keys, not a setting that is silently dropped. Both keys must be
/// strings when present.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
            Err(source) => return Err(ConfigError::Unreadable { path, source }),
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

/// An unknown key found in a config section: its dotted path under the
/// section and the keys that would have been valid at that level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    pub path: String,
    pub valid: Vec<String>,
}

/// Keys of `actual` (a section as read from the file) that `known` (the JSON
/// of the section's struct at its defaults) does not have, recursing into
/// nested mappings. A key whose default is not a mapping (a scalar, a list,
/// an absent `Option`) is a leaf: whatever the file puts there is that key's
/// business. Sorted, so the output is stable.
pub fn unknown_keys(actual: &serde_json::Value, known: &serde_json::Value) -> Vec<UnknownKey> {
    fn walk(
        actual: &serde_json::Value,
        known: &serde_json::Value,
        prefix: &str,
        out: &mut Vec<UnknownKey>,
    ) {
        let (Some(actual), Some(known)) = (actual.as_object(), known.as_object()) else {
            return;
        };
        for (key, value) in actual {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match known.get(key) {
                None => out.push(UnknownKey {
                    path,
                    valid: known.keys().cloned().collect(),
                }),
                Some(known_value) => walk(value, known_value, &path, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(actual, known, "", &mut out);
    // serde_json's `preserve_order` feature (unified in by another workspace
    // crate) makes object iteration follow the file; the contract is sorted.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// `true` the first time `key` is seen in this process. Config loaders run
/// several times per command; a warning about the same file is logged once.
fn first_time(key: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex};
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string())
}

/// One line per unknown key in `section` (minus `ignore`: keys handled
/// elsewhere, such as keys known to be removed, which have their own
/// warning), each naming the file, the dotted key and the valid keys. For
/// callers that cannot log yet (the logger is not installed when the
/// `logs:` section is read) and carry the lines to where they can.
pub fn unknown_key_notices(
    file: &Path,
    section: &str,
    actual: &serde_json::Value,
    known: &serde_json::Value,
    ignore: &[&str],
) -> Vec<String> {
    unknown_keys(actual, known)
        .into_iter()
        .filter(|u| !ignore.contains(&u.path.as_str()))
        .map(|u| {
            format!(
                "{}: unknown config key `{section}.{}` is ignored (a typo, or a key that no \
                 longer exists); valid keys here: {}",
                file.display(),
                u.path,
                u.valid.join(", ")
            )
        })
        .collect()
}

/// Log (once per process and key) a warning for every unknown key in
/// `section` (see [`unknown_key_notices`]). These sections tolerate stale
/// keys (deployed configs carry some), so the finding is a warning.
pub fn warn_unknown_keys(
    file: &Path,
    section: &str,
    actual: &serde_json::Value,
    known: &serde_json::Value,
    ignore: &[&str],
) {
    for notice in unknown_key_notices(file, section, actual, known, ignore) {
        if first_time(&notice) {
            tracing::warn!("{notice}");
        }
    }
}

/// Names among `names` that start with `prefix` (e.g. `HS_SCRIBE_`), are not
/// in `allow` (variables in that namespace that are not config keys, such as
/// `HS_SCRIBE_DIAG_DIR`) and match no top-level key of any tree in `known`.
/// Sorted.
pub fn unknown_prefixed_env(
    prefix: &str,
    known: &[&serde_json::Value],
    allow: &[&str],
    names: impl Iterator<Item = String>,
) -> Vec<String> {
    let mut unknown: Vec<String> = names
        .filter(|name| name.starts_with(prefix) && !allow.contains(&name.as_str()))
        .filter(|name| {
            let key = name[prefix.len()..].to_ascii_lowercase();
            !known
                .iter()
                .any(|tree| tree.as_object().is_some_and(|o| o.contains_key(&key)))
        })
        .collect();
    unknown.sort();
    unknown
}

/// Every `<prefix>[A-Z0-9_]+` in `text`, sorted and de-duplicated: the
/// environment variables a document (README, compose file, unit template)
/// tells an operator about. A bare prefix, as in prose about "the
/// `HS_SCRIBE_` prefix", is not a variable. Lets a test hold documentation
/// to the variables the loaders actually read.
pub fn env_names_in_text(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let tail = &rest[at..];
        let len = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..len];
        if name.len() > prefix.len() && !name.ends_with('_') {
            out.push(name.to_string());
        }
        rest = &tail[len.max(1)..];
    }
    out.sort();
    out.dedup();
    out
}

/// Log (once per process and name) a warning for every `prefix*` environment
/// variable that sets nothing: such a variable is dropped by the loaders, so
/// an operator who sets it believes in an effect it does not have.
pub fn warn_unknown_prefixed_env(prefix: &str, known: &[&serde_json::Value], allow: &[&str]) {
    let unknown = unknown_prefixed_env(
        prefix,
        known,
        allow,
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
    );
    for name in unknown {
        if first_time(&format!("env#{name}")) {
            let mut valid: Vec<String> = known
                .iter()
                .filter_map(|tree| tree.as_object())
                .flat_map(|o| o.keys())
                .map(|k| format!("{prefix}{}", k.to_ascii_uppercase()))
                .collect();
            valid.sort();
            valid.dedup();
            tracing::warn!(
                variable = %name,
                valid = %valid.join(", "),
                "environment variable sets nothing and is ignored; fix or unset it"
            );
        }
    }
}

#[cfg(feature = "storage")]
impl ConfigFile {
    /// The `storage:` section, or the documented default (local filesystem
    /// at `~/home-still`) when the section is absent. An absent section on a
    /// host whose `home.project_dir` points elsewhere is logged once (see
    /// [`Self::default_storage`]).
    pub fn storage(&self) -> Result<crate::storage::StorageConfig, ConfigError> {
        match self.section("storage")? {
            Some(storage) => Ok(storage),
            None => self.default_storage(),
        }
    }

    /// The default storage for a file with no `storage:` section. The
    /// default root is `~/home-still` whatever `home.project_dir` says;
    /// moving it would silently relocate the objects of every host that runs
    /// on today's default, so it is only announced: once, naming both paths.
    pub fn default_storage(&self) -> Result<crate::storage::StorageConfig, ConfigError> {
        let storage = crate::storage::StorageConfig::default();
        if let Some(note) = self.storage_default_note(&storage)? {
            if first_time(&format!("storage-default#{}", self.path.display())) {
                tracing::warn!("{note}");
            }
        }
        Ok(storage)
    }

    fn storage_default_note(
        &self,
        storage: &crate::storage::StorageConfig,
    ) -> Result<Option<String>, ConfigError> {
        let project = self.project_dir()?;
        if project == self.home.join(PROJECT_DIR_DEFAULT) {
            return Ok(None);
        }
        Ok(Some(format!(
            "{}: no `storage:` section, so objects (papers, markdown, catalog) are stored under \
             {} while `home.project_dir` is {}; add `storage.local.root` (or `storage.backend: \
             s3`) so both agree, or confirm the split is intended",
            self.path.display(),
            storage.local.root.display(),
            project.display()
        )))
    }
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
        assert!(file
            .section::<serde_yaml_ng::Value>("storage")
            .unwrap()
            .is_none());
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

    #[test]
    fn unknown_keys_in_home_are_errors_naming_the_key_and_the_valid_ones() {
        for yaml in [
            "home:\n  project_directory: /x\n",
            "home:\n  project_dir: /x\n  logdir: /y\n",
        ] {
            let home = home_with(Some(yaml));
            let file = ConfigFile::load_in(home.path()).unwrap();
            let err = file.project_dir().unwrap_err().to_string();
            assert!(err.contains("`home`"), "{yaml}: {err}");
            assert!(
                err.contains("project_directory") || err.contains("logdir"),
                "{err}"
            );
            assert!(
                err.contains("project_dir") && err.contains("log_dir"),
                "{err}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_found_at_any_depth_and_leaves_are_not_descended_into() {
        let known = serde_json::json!({
            "a": 1, "list": [], "maybe": null,
            "nested": { "x": 1, "y": { "z": 1 } },
        });
        let actual = serde_json::json!({
            "a": 2, "typo": 1, "list": [{"anything": 1}], "maybe": {"free": "form"},
            "nested": { "x": 2, "w": 3, "y": { "z": 2, "q": 4 } },
        });
        let found: Vec<String> = unknown_keys(&actual, &known)
            .into_iter()
            .map(|u| u.path)
            .collect();
        assert_eq!(found, ["nested.w", "nested.y.q", "typo"]);
        let top = unknown_keys(&actual, &known)
            .into_iter()
            .find(|u| u.path == "typo")
            .unwrap();
        assert_eq!(top.valid, ["a", "list", "maybe", "nested"]);
        assert!(unknown_keys(&known, &known).is_empty());
    }

    #[test]
    fn unknown_prefixed_env_skips_known_keys_in_any_tree_and_the_allowlist() {
        let client = serde_json::json!({ "convert_timeout_secs": 1, "servers": [] });
        let server = serde_json::json!({ "vlm_concurrency": 1 });
        let names = [
            "HS_SCRIBE_CONVERT_TIMEOUT_SECS",
            "HS_SCRIBE_VLM_CONCURRENCY",
            "HS_SCRIBE_TIMEOUT_SECS",
            "HS_SCRIBE_DIAG_DIR",
            "HS_SCRIBE_vlm_concurency",
            "HS_OTHER_X",
        ]
        .map(String::from);
        let unknown = unknown_prefixed_env(
            "HS_SCRIBE_",
            &[&client, &server],
            &["HS_SCRIBE_DIAG_DIR"],
            names.into_iter(),
        );
        assert_eq!(
            unknown,
            ["HS_SCRIBE_TIMEOUT_SECS", "HS_SCRIBE_vlm_concurency"]
        );
    }

    #[test]
    fn a_warning_is_logged_once_per_key() {
        assert!(first_time("test-unique-key-a"));
        assert!(!first_time("test-unique-key-a"));
        assert!(first_time("test-unique-key-b"));
    }

    #[test]
    fn debug_output_names_sections_and_never_values() {
        let home = home_with(Some(
            "storage:\n  s3:\n    secret_key: hunter2-secret\nevents:\n  nats:\n    user: bob\n",
        ));
        let file = ConfigFile::load_in(home.path()).unwrap();
        let shown = format!("{file:?} {file:#?}");
        assert!(
            shown.contains("storage") && shown.contains("events"),
            "{shown}"
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("bob"),
            "{shown}"
        );
    }

    #[test]
    fn the_variable_scanner_finds_variables_and_not_bare_prefixes() {
        let text = "with the `HS_SCRIBE_` prefix set HS_SCRIBE_DPI=150 and\n  HS_SCRIBE_USE_CUDA: \"false\" (HS_SCRIBE_<KEY>) HS_SCRIBE_DPI";
        assert_eq!(
            env_names_in_text(text, "HS_SCRIBE_"),
            ["HS_SCRIBE_DPI", "HS_SCRIBE_USE_CUDA"]
        );
        assert!(env_names_in_text("no variables here", "HS_SCRIBE_").is_empty());
    }

    #[cfg(feature = "storage")]
    mod storage {
        use super::*;

        fn note(config: Option<&str>) -> (tempfile::TempDir, Option<String>) {
            let home = home_with(config);
            let file = ConfigFile::load_in(home.path()).unwrap();
            let storage = crate::storage::StorageConfig::default();
            let note = file.storage_default_note(&storage).unwrap();
            (home, note)
        }

        #[test]
        fn an_absent_storage_section_is_silent_on_the_default_project_dir() {
            assert!(note(None).1.is_none());
            assert!(note(Some("home:\n  log_dir: /var/log/hs\n")).1.is_none());
        }

        #[test]
        fn an_absent_storage_section_with_a_moved_project_dir_names_both_paths() {
            let (_home, note) = note(Some("home:\n  project_dir: /data/hs\n"));
            let note = note.expect("must be announced");
            let default_root = crate::storage::StorageConfig::default().local.root;
            assert!(note.contains("/data/hs"), "{note}");
            assert!(note.contains(&default_root.display().to_string()), "{note}");
            assert!(note.contains("storage.local.root"), "{note}");
        }

        #[test]
        fn the_default_root_is_not_moved_by_the_warning() {
            // Only announced: relocating it would orphan the objects of
            // every host that runs on today's default.
            let home = home_with(Some("home:\n  project_dir: /data/hs\n"));
            let file = ConfigFile::load_in(home.path()).unwrap();
            assert_eq!(
                file.storage().unwrap().local.root,
                crate::storage::StorageConfig::default().local.root
            );
        }

        #[test]
        fn unknown_keys_in_storage_are_errors_naming_the_key() {
            // `storage.root` instead of `storage.local.root` used to run on
            // the default root without a word.
            for yaml in [
                "storage:\n  backend: local\n  root: /x\n",
                "storage:\n  local:\n    rooot: /x\n",
                "storage:\n  backend: s3\n  s3:\n    secret: x\n",
            ] {
                let home = home_with(Some(yaml));
                let file = ConfigFile::load_in(home.path()).unwrap();
                let err = file.storage().unwrap_err().to_string();
                assert!(err.contains("`storage`"), "{yaml}: {err}");
                assert!(err.contains("unknown field"), "{yaml}: {err}");
            }
            let home = home_with(Some("storage:\n  backend: local\n  local:\n    root: /x\n"));
            let file = ConfigFile::load_in(home.path()).unwrap();
            assert_eq!(
                file.storage().unwrap().local.root,
                std::path::PathBuf::from("/x")
            );
        }
    }
}
