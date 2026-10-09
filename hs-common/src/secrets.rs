//! Parse `~/.home-still/secrets.env` and export its entries as process env vars.
//!
//! Every `hs-*` binary calls [`load_default_secrets`] at the top of `main` so
//! that spawn contexts which don't inherit a shell (Claude Desktop, launchd,
//! `npx mcp-remote`, direct exec) still see the secrets that `hs config init`
//! wrote. Existing env vars are never overridden — explicit process env wins.

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

const REL_PATH: &str = "secrets.env";

/// Where `secrets.env` lives: `$HOME/.home-still/secrets.env`.
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(crate::HIDDEN_DIR).join(REL_PATH))
}

/// Parse `secrets.env` (if present at the default path) and `set_var` any
/// entry that isn't already in the environment. Returns the number of keys
/// newly exported. Missing file is not an error.
pub fn load_default_secrets() -> io::Result<usize> {
    match default_path() {
        Some(p) => load_secrets_from(&p),
        None => Ok(0),
    }
}

/// Same as [`load_default_secrets`] but reads from an explicit path.
pub fn load_secrets_from(path: &Path) -> io::Result<usize> {
    let entries = match parse_secrets_from_path(path)? {
        Some(e) => e,
        None => return Ok(0),
    };
    check_mode_warn(path);
    let mut exported = 0;
    for (k, v) in select_unset(entries, |k| std::env::var_os(k).is_some()) {
        // SAFETY: set_var is unsafe in edition-2024 preview because of
        // multi-threaded mutation; we call this before any tokio runtime
        // starts so the process is still single-threaded.
        unsafe { std::env::set_var(&k, v) };
        exported += 1;
    }
    Ok(exported)
}

/// The entries whose key `is_set` reports as absent from the environment —
/// explicit process env always wins over the file.
fn select_unset(
    entries: HashMap<String, String>,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    entries.into_iter().filter(|(k, _)| !is_set(k)).collect()
}

/// Parse `secrets.env` at `path` and return its `KEY=VALUE` pairs. Returns
/// `Ok(None)` if the file doesn't exist; other IO errors bubble up.
pub fn parse_secrets_from_path(path: &Path) -> io::Result<Option<HashMap<String, String>>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut out = HashMap::new();
    for (idx, line) in io::BufReader::new(file).lines().enumerate() {
        let line = line?;
        if let Some((k, v)) = parse_line(&line) {
            // `set_var` panics on a NUL; refuse the file instead. Only the
            // position is named, never the entry: it may be a secret.
            if k.contains('\0') || v.contains('\0') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: entry contains a NUL byte", path.display(), idx + 1),
                ));
            }
            out.insert(k, v);
        }
    }
    Ok(Some(out))
}

fn parse_line(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (mut key, rest) = trimmed.split_once('=')?;
    key = key.trim();
    if key.is_empty() {
        return None;
    }
    // Strip optional `export ` prefix so `export KEY=VALUE` also works.
    let stripped_key = key
        .strip_prefix("export ")
        .or_else(|| key.strip_prefix("export\t"))
        .map(str::trim)
        .unwrap_or(key);
    let rest = rest.trim();
    // A quoted value may be followed by a comment: `KEY="value" # note`. The
    // closing quote ends the value; anything else after it is not a comment,
    // and the value falls through to the literal handling below.
    if let Some(quote @ ('"' | '\'')) = rest.chars().next() {
        if let Some(close) = rest[1..].find(quote) {
            let after = rest[1 + close + 1..].trim_start();
            if after.is_empty() || after.starts_with('#') {
                return Some((stripped_key.to_string(), rest[1..1 + close].to_string()));
            }
        }
    }
    let mut val = rest.to_string();
    // Strip a trailing ` # comment` only when the value isn't quoted.
    if !val.starts_with('"') && !val.starts_with('\'') {
        if let Some(hash) = val.find(" #") {
            val.truncate(hash);
            val = val.trim_end().to_string();
        }
    }
    // Strip matching surrounding quotes.
    if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
        || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
    {
        val = val[1..val.len() - 1].to_string();
    }
    Some((stripped_key.to_string(), val))
}

#[cfg(unix)]
fn check_mode_warn(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: {} is mode {:o}; run `chmod 600` to restrict",
                path.display(),
                mode,
            );
        }
    }
}

#[cfg(not(unix))]
fn check_mode_warn(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("secrets.env");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        (tmp, p)
    }

    #[test]
    fn parses_plain_key_value() {
        let (_t, p) = write_tmp("FOO=bar\nBAZ=qux\n");
        let map = parse_secrets_from_path(&p).unwrap().unwrap();
        assert_eq!(map.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(map.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn tolerates_comments_blanks_and_quotes() {
        let (_t, p) = write_tmp(
            "# leading comment\n\
             \n\
             KEY_A=plain\n\
             KEY_B=\"with spaces\"\n\
             KEY_C='single quotes'\n\
             KEY_D=trailing # comment\n\
             export KEY_E=exported\n",
        );
        let map = parse_secrets_from_path(&p).unwrap().unwrap();
        assert_eq!(map["KEY_A"], "plain");
        assert_eq!(map["KEY_B"], "with spaces");
        assert_eq!(map["KEY_C"], "single quotes");
        assert_eq!(map["KEY_D"], "trailing");
        assert_eq!(map["KEY_E"], "exported");
    }

    #[test]
    fn a_comment_after_a_quoted_value_is_not_part_of_the_value() {
        let (_t, p) = write_tmp(
            "TOKEN=\"abc123\" # rotated 2026-10\n\
             SINGLE='x y'   #note\n\
             HASH_INSIDE=\"keep # this\"\n\
             JUNK_AFTER=\"a\"b\"\n",
        );
        let map = parse_secrets_from_path(&p).unwrap().unwrap();
        assert_eq!(map["TOKEN"], "abc123");
        assert_eq!(map["SINGLE"], "x y");
        assert_eq!(map["HASH_INSIDE"], "keep # this");
        assert_eq!(map["JUNK_AFTER"], "a\"b");
    }

    #[test]
    fn a_nul_byte_is_an_error_that_does_not_echo_the_entry() {
        let (_t, p) = write_tmp("OK=1\nSECRET_KEY=hunter2\0tail\n");
        let err = parse_secrets_from_path(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains(":2:"), "{msg}");
        assert!(
            !msg.contains("hunter2") && !msg.contains("SECRET_KEY"),
            "{msg}"
        );
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.env");
        assert!(parse_secrets_from_path(&missing).unwrap().is_none());
    }

    #[test]
    fn load_does_not_overwrite_existing_env() {
        let (_t, p) =
            write_tmp("HS_TEST_SECRETS_LOADER_A=from_file\nHS_TEST_SECRETS_LOADER_B=from_file\n");
        let entries = parse_secrets_from_path(&p).unwrap().unwrap();
        // A is already in the environment, B is not.
        let chosen = select_unset(entries, |k| k == "HS_TEST_SECRETS_LOADER_A");
        assert_eq!(
            chosen,
            vec![(
                "HS_TEST_SECRETS_LOADER_B".to_string(),
                "from_file".to_string()
            )],
            "only B should be newly exported"
        );
    }
}
