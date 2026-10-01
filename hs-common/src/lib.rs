pub mod exit_codes;
pub mod gpu;
pub mod hardware_profile;
pub mod html;
#[cfg(feature = "http")]
pub mod http;
pub mod mode;
pub mod pipe_reporter;
pub mod quality;
pub mod reporter;
pub mod secrets;

/// Relative path from $HOME to the config file.
pub const CONFIG_REL_PATH: &str = ".home-still/config.yaml";

/// Hidden directory for config, cache, models (relative to $HOME).
pub const HIDDEN_DIR: &str = ".home-still";

/// Visible project directory for papers, markdown (relative to $HOME).
pub const PROJECT_DIR_DEFAULT: &str = "home-still";

/// Resolve the project directory from config (home.project_dir) or default ~/home-still.
/// This reads the config file directly to avoid heavy YAML parser dependencies.
pub fn resolve_project_dir() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(CONFIG_REL_PATH);

    // Try to read project_dir from config file
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        // Simple line-by-line scan for project_dir
        let mut in_home_section = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            // Track YAML sections: top-level keys have no leading whitespace
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_home_section = trimmed.starts_with("home:");
            }
            if in_home_section {
                if let Some(val) = trimmed.strip_prefix("project_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        if let Some(rest) = val.strip_prefix("~/") {
                            return home.join(rest);
                        }
                        return std::path::PathBuf::from(val);
                    }
                }
            }
        }
    }

    // Default: ~/home-still
    home.join(PROJECT_DIR_DEFAULT)
}

/// Shard prefix of a stem: the leading characters that fit in 2 bytes,
/// never splitting a UTF-8 character and never empty for a non-empty stem.
///
/// For ASCII stems this is the first 2 characters (`ab` for `abc`, `a` for
/// `a`). A stem whose first character is 2 bytes wide (`école`) yields that
/// one character, exactly what the old `&stem[..2]` produced, so every key
/// that was ever written without panicking keeps its location. A stem whose
/// byte 2 falls inside a multi-byte character (`Müller`, `中文`) used to
/// panic; it now yields the characters before that point (`M`) or, when the
/// first character alone is wider than 2 bytes, just that character (`中`).
fn shard_prefix(stem: &str) -> &str {
    let mut end = 0;
    for c in stem.chars() {
        let next = end + c.len_utf8();
        if end > 0 && next > 2 {
            break;
        }
        end = next;
        if end >= 2 {
            break;
        }
    }
    &stem[..end]
}

/// Build a sharded path: `dir/{prefix}/{stem}.{ext}` where prefix is the
/// first 2 bytes of the stem, rounded down to a character boundary (at least
/// one character). This keeps any single directory from growing beyond a few
/// hundred entries, which fixes macOS Finder NFS browsing and improves
/// readdir performance in general.
///
/// Does not validate `stem`; call [`validate_stem`] first on untrusted input.
pub fn sharded_path(dir: &std::path::Path, stem: &str, ext: &str) -> std::path::PathBuf {
    dir.join(shard_prefix(stem)).join(format!("{stem}.{ext}"))
}

/// Build a sharded storage key: `{prefix}/{stem}.{ext}` — the Storage-trait
/// equivalent of `sharded_path` (no root prefix; keys are root-relative).
///
/// Never panics, but does not validate `stem` either: an empty stem yields an
/// absolute-looking key and `..` a traversal key. Call [`validate_stem`] at
/// every untrusted boundary (CLI file names, MCP arguments, event payloads).
pub fn sharded_key(stem: &str, ext: &str) -> String {
    format!("{}/{stem}.{ext}", shard_prefix(stem))
}

/// Why [`validate_stem`] rejected a stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidStem {
    Empty,
    /// `.` or `..`
    DotSegment,
    /// `/` or `\`
    PathSeparator,
    Nul,
}

impl std::fmt::Display for InvalidStem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "stem is empty",
            Self::DotSegment => "stem is '.' or '..'",
            Self::PathSeparator => "stem contains a path separator",
            Self::Nul => "stem contains a NUL byte",
        })
    }
}

impl std::error::Error for InvalidStem {}

/// Check that `stem` is a single, plain file-name component, safe to feed to
/// [`sharded_key`] / [`sharded_path`] and to splice into a storage key.
///
/// Rejects the empty string, `.`, `..`, `/`, `\` and NUL. Everything else
/// (non-ASCII, spaces, leading dots such as `.hidden`) is a valid stem.
/// Call this at every untrusted boundary before the stem reaches storage.
pub fn validate_stem(stem: &str) -> Result<(), InvalidStem> {
    if stem.is_empty() {
        return Err(InvalidStem::Empty);
    }
    if stem == "." || stem == ".." {
        return Err(InvalidStem::DotSegment);
    }
    if stem.contains(['/', '\\']) {
        return Err(InvalidStem::PathSeparator);
    }
    if stem.contains('\0') {
        return Err(InvalidStem::Nul);
    }
    Ok(())
}

/// Recursively collect all files with a given extension under `dir`.
pub fn collect_files_recursive(dir: &std::path::Path, ext: &str) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    fn walk(dir: &std::path::Path, ext: &str, result: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, ext, result);
                } else if path.extension().is_some_and(|e| e == ext) {
                    result.push(path);
                }
            }
        }
    }
    walk(dir, ext, &mut result);
    result
}

/// Resolve the log directory from config (home.log_dir) or default {project_dir}/logs.
pub fn resolve_log_dir() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(CONFIG_REL_PATH);

    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        let mut in_home_section = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_home_section = trimmed.starts_with("home:");
            }
            if in_home_section {
                if let Some(val) = trimmed.strip_prefix("log_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        if let Some(rest) = val.strip_prefix("~/") {
                            return home.join(rest);
                        }
                        return std::path::PathBuf::from(val);
                    }
                }
            }
        }
    }

    resolve_project_dir().join("logs")
}

#[cfg(feature = "cli")]
pub mod global_args;
#[cfg(feature = "cli")]
pub mod styles;
#[cfg(feature = "cli")]
pub mod tty_reporter;

#[cfg(feature = "service")]
pub mod service;

#[cfg(feature = "catalog")]
pub mod catalog;

pub mod status;

#[cfg(feature = "storage")]
pub mod storage;

#[cfg(feature = "storage")]
pub mod markdown;

#[cfg(feature = "events")]
pub mod event_bus;

#[cfg(all(feature = "storage", feature = "events"))]
pub mod inbox;

#[cfg(feature = "logging")]
pub mod logging;

#[cfg(feature = "compose")]
pub mod compose;

#[cfg(feature = "auth")]
pub mod auth;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharded_key_ascii_matches_two_byte_prefix() {
        assert_eq!(sharded_key("abcdef", "pdf"), "ab/abcdef.pdf");
        assert_eq!(sharded_key("ab", "md"), "ab/ab.md");
        assert_eq!(sharded_key("a", "md"), "a/a.md");
    }

    #[test]
    fn sharded_key_never_panics_on_multibyte_stems() {
        // Byte 2 inside a multi-byte char: used to panic in `&stem[..2]`.
        assert_eq!(sharded_key("Müller", "pdf"), "M/Müller.pdf");
        assert_eq!(sharded_key("Año", "pdf"), "A/Año.pdf");
        assert_eq!(sharded_key("Cómo", "pdf"), "C/Cómo.pdf");
        // Multi-byte char starting at byte 1 and at byte 0.
        assert_eq!(sharded_key("aé", "md"), "a/aé.md");
        assert_eq!(sharded_key("中文研究", "md"), "中/中文研究.md");
        assert_eq!(sharded_key("🦀crab", "md"), "🦀/🦀crab.md");
    }

    #[test]
    fn sharded_key_keeps_prefix_for_stems_that_never_panicked() {
        // First char is exactly 2 bytes: `&stem[..2]` was valid and produced
        // this prefix, so existing objects must stay addressable.
        assert_eq!(sharded_key("école", "pdf"), "é/école.pdf");
        assert_eq!(sharded_key("ñandú", "pdf"), "ñ/ñandú.pdf");
    }

    #[test]
    fn sharded_key_prefix_is_never_empty_for_non_empty_stems() {
        for stem in ["a", "é", "中", "🦀", "aé", "a中", "éé", "..", "x"] {
            let key = sharded_key(stem, "pdf");
            let (prefix, rest) = key.split_once('/').unwrap();
            assert!(!prefix.is_empty(), "empty prefix for {stem:?}");
            assert!(
                stem.starts_with(prefix),
                "{prefix:?} not a prefix of {stem:?}"
            );
            assert_eq!(rest, format!("{stem}.pdf"));
        }
    }

    #[test]
    fn sharded_path_matches_sharded_key() {
        let dir = std::path::Path::new("/data");
        for stem in ["abc", "Müller", "中文", "é", "a"] {
            let from_path = sharded_path(dir, stem, "pdf");
            let from_key = std::path::Path::new("/data").join(sharded_key(stem, "pdf"));
            assert_eq!(from_path, from_key, "{stem:?}");
        }
    }

    #[test]
    fn validate_stem_accepts_plain_and_non_ascii_names() {
        for stem in [
            "10.1000_xyz",
            "Müller",
            "Año 2020 (final)",
            "中文",
            ".hidden",
            "a.b.c",
            "10.1016%2Fj.cell",
            "...",
        ] {
            assert_eq!(validate_stem(stem), Ok(()), "{stem:?}");
        }
    }

    #[test]
    fn validate_stem_rejects_traversal_and_separators() {
        assert_eq!(validate_stem(""), Err(InvalidStem::Empty));
        assert_eq!(validate_stem("."), Err(InvalidStem::DotSegment));
        assert_eq!(validate_stem(".."), Err(InvalidStem::DotSegment));
        for stem in ["a/b", "/abs", "../x", "x/..", "a\\b", "..\\x", "/"] {
            assert_eq!(
                validate_stem(stem),
                Err(InvalidStem::PathSeparator),
                "{stem:?}"
            );
        }
        assert_eq!(validate_stem("a\0b"), Err(InvalidStem::Nul));
    }
}
