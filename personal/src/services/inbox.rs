//! Filename → inbox-path resolver, used by the MCP `personal_add` tool to
//! constrain ingestion to files staged under `cfg.inbox_dir()`. The MCP
//! surface accepts only filenames (basenames); this module is the single
//! point where a name is turned into a path, and where escape attempts are
//! rejected. The CLI `hs personal add` keeps taking arbitrary paths — that's
//! the user-driven path and is not subject to inbox restriction.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use std::path::PathBuf;

/// Resolve `filename` to an absolute path inside the inbox. Rejects any name
/// that contains a path separator, a leading dot, an absolute prefix, or
/// resolves outside the inbox via symlink/`..`. Auto-creates the inbox
/// directory on first use so a fresh install doesn't trip on a missing dir.
pub fn resolve_inbox_path(cfg: &Config, filename: &str) -> Result<PathBuf> {
    let inbox = cfg.inbox_dir();
    if !inbox.exists() {
        std::fs::create_dir_all(&inbox)?;
    }

    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "filename is required and must not be empty"
        )));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "filename must not contain path separators (got '{trimmed}')"
        )));
    }
    if trimmed.starts_with('.') {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "filename must not start with '.' (got '{trimmed}')"
        )));
    }

    let target = inbox.join(trimmed);
    if !target.exists() {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "no such file in inbox: '{trimmed}' (drop it under {})",
            inbox.display()
        )));
    }

    // canonicalize() resolves symlinks; verify the result is still under
    // the canonicalized inbox so a symlink in the inbox can't smuggle in
    // /etc/shadow or ~/.ssh/id_rsa.
    let canonical_inbox = inbox.canonicalize()?;
    let canonical_target = target.canonicalize()?;
    if !canonical_target.starts_with(&canonical_inbox) {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "filename '{trimmed}' resolves outside the inbox (symlink escape?)"
        )));
    }

    Ok(canonical_target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cfg_with_inbox(path: &std::path::Path) -> Config {
        Config {
            ingest_inbox: path.display().to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn resolves_simple_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("note.txt"), b"hello").unwrap();

        let cfg = cfg_with_inbox(&inbox);
        let p = resolve_inbox_path(&cfg, "note.txt").unwrap();
        assert_eq!(p, inbox.canonicalize().unwrap().join("note.txt"));
    }

    #[test]
    fn rejects_path_separators() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_inbox(tmp.path());
        assert!(resolve_inbox_path(&cfg, "sub/file.txt").is_err());
        assert!(resolve_inbox_path(&cfg, "..\\file.txt").is_err());
        assert!(resolve_inbox_path(&cfg, "/etc/passwd").is_err());
    }

    #[test]
    fn rejects_dotfiles_and_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_inbox(tmp.path());
        assert!(resolve_inbox_path(&cfg, ".env").is_err());
        assert!(resolve_inbox_path(&cfg, "..").is_err());
    }

    #[test]
    fn rejects_empty_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_inbox(tmp.path());
        assert!(resolve_inbox_path(&cfg, "").is_err());
        assert!(resolve_inbox_path(&cfg, "   ").is_err());
    }

    #[test]
    fn rejects_missing_file_with_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_inbox(tmp.path());
        let err = resolve_inbox_path(&cfg, "nope.txt").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no such file"), "got: {msg}");
    }

    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&inbox).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"shhh").unwrap();
        // Symlink inside the inbox pointing at a file outside it.
        std::os::unix::fs::symlink(&secret, inbox.join("link.txt")).unwrap();

        let cfg = cfg_with_inbox(&inbox);
        let err = resolve_inbox_path(&cfg, "link.txt").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("outside the inbox"),
            "expected escape rejection, got: {msg}"
        );
    }
}
