//! Storage-backed access to converted markdown documents.
//!
//! Mirror of `crate::catalog`'s `*_via` helpers, but for `.md` payloads that
//! don't need deserialization. `prefix` is the sub-path inside the storage
//! backend where markdown files live (e.g. `"markdown"` under a project root,
//! or `""` for a dedicated bucket).

use crate::storage::{ObjectMeta, Storage};

/// Default storage prefix for markdown objects. Every writer and reader
/// should agree on this so `markdown/{shard}/{stem}.md` is the one true
/// convention across MCP tools, event-bus daemons, and direct Storage calls.
pub const MARKDOWN_PREFIX: &str = "markdown";

/// Fully-qualified storage key for a markdown document under the default
/// `markdown/` prefix. Shape: `markdown/{shard}/{stem}.md`.
pub fn markdown_storage_key(stem: &str) -> String {
    markdown_key(MARKDOWN_PREFIX, stem)
}

fn markdown_key(prefix: &str, stem: &str) -> String {
    let key = crate::sharded_key(stem, "md");
    if prefix.is_empty() {
        key
    } else {
        format!("{}/{}", prefix.trim_end_matches('/'), key)
    }
}

/// Resolve the storage key for a doc_id's markdown object (pure derivation).
///
/// Prefers the catalog-recorded path if present. Falls back to the sharded
/// derivation when the catalog has no recorded path.
///
/// Use the async [`resolve_markdown_key_verified`] for read paths that
/// actually need to find the file — `bc2b6fb` sharded the physical layout
/// and pre-rc.241 catalog rows still carry stale unsharded `markdown_path`
/// values. Trusting them blindly produces ghost orphans.
pub fn resolve_markdown_key(prefix: &str, stem: &str, stored_path: Option<&str>) -> String {
    stored_path
        .map(|p| p.to_string())
        .unwrap_or_else(|| markdown_key(prefix, stem))
}

/// Verified resolution: probe the catalog-recorded `stored_path` only when
/// it actually exists on storage; otherwise return the sharded canonical
/// key.
///
/// Fixes the 2026-04 ghost-orphan regression where rc.241 taught the
/// distill read paths to trust `catalog_entry.markdown_path` verbatim.
/// Pre-`bc2b6fb` rows still record `markdown/<stem>.md` (unsharded) even
/// though the file itself was migrated to `markdown/{XX}/{stem}.md`
/// (sharded); `resolve_markdown_key` returned the stale unsharded path and
/// `storage.exists()` always said false, flagging ~2,727 valid doc_ids as
/// orphans in `distill_reconcile`.
///
/// Does at most one extra HEAD (skipped when `stored_path` matches the
/// sharded derivation, or is `None`). Callers still need to check
/// existence of the returned key — this helper only decides *which* key
/// is worth checking.
pub async fn resolve_markdown_key_verified(
    storage: &dyn Storage,
    prefix: &str,
    stem: &str,
    stored_path: Option<&str>,
) -> String {
    let sharded = markdown_key(prefix, stem);
    if let Some(p) = stored_path {
        if p != sharded && storage.exists(p).await.unwrap_or(false) {
            return p.to_string();
        }
    }
    sharded
}

/// List the stems of every markdown document under `prefix`.
pub async fn list_markdown_stems_via(
    storage: &dyn Storage,
    prefix: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(list_markdown_meta_via(storage, prefix)
        .await?
        .into_iter()
        .map(|(stem, _)| stem)
        .collect())
}

/// List every markdown document under `prefix` along with its `ObjectMeta`
/// (size, last-modified). Useful for the `markdown_list` tool handler —
/// sizes come straight from the Storage listing, no extra roundtrip.
pub async fn list_markdown_meta_via(
    storage: &dyn Storage,
    prefix: &str,
) -> anyhow::Result<Vec<(String, ObjectMeta)>> {
    let objects = storage.list(prefix).await?;
    let mut out = Vec::with_capacity(objects.len());
    for obj in objects {
        if !obj.key.ends_with(".md") {
            continue;
        }
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        if filename.starts_with("._") {
            continue;
        }
        let stem = filename.trim_end_matches(".md").to_string();
        out.push((stem, obj));
    }
    Ok(out)
}

/// Read a single markdown document by stem. Returns `None` if the object
/// doesn't exist. Reads the full document — callers that only need a
/// specific page range should still do that locally after the fetch (same
/// behavior as the filesystem variant in the MCP handler).
pub async fn read_markdown_via(storage: &dyn Storage, prefix: &str, stem: &str) -> Option<String> {
    let key = markdown_key(prefix, stem);
    let bytes = storage.get(&key).await.ok()?;
    String::from_utf8(bytes).ok()
}

/// True if the named markdown document exists.
pub async fn markdown_exists_via(
    storage: &dyn Storage,
    prefix: &str,
    stem: &str,
) -> anyhow::Result<bool> {
    let key = markdown_key(prefix, stem);
    storage.exists(&key).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalFsStorage;

    #[test]
    fn resolve_uses_catalog_path_when_present() {
        // Pre-rc.241 unsharded row: catalog points at `markdown/<stem>.md`,
        // resolver MUST return that exact path — not re-derive to sharded.
        let got = resolve_markdown_key(
            "markdown",
            "10.1002_aur.2162",
            Some("markdown/10.1002_aur.2162.md"),
        );
        assert_eq!(got, "markdown/10.1002_aur.2162.md");
    }

    #[test]
    fn resolve_falls_back_to_sharded_when_catalog_path_missing() {
        let got = resolve_markdown_key("markdown", "10.1002_aur.2162", None);
        assert_eq!(got, "markdown/10/10.1002_aur.2162.md");
    }

    #[test]
    fn resolve_handles_empty_prefix() {
        let got = resolve_markdown_key("", "abcdef", None);
        assert_eq!(got, "ab/abcdef.md");
    }

    #[test]
    fn resolve_respects_alternate_sharded_catalog_path() {
        // A row may have its markdown under a different top-level prefix
        // (e.g. a migration that moved objects). Resolver trusts the stored
        // path, never second-guesses it.
        let got = resolve_markdown_key("markdown", "ab", Some("legacy/ab.md"));
        assert_eq!(got, "legacy/ab.md");
    }

    #[tokio::test]
    async fn verified_returns_sharded_when_stored_is_stale() {
        // The 2026-04 ghost-orphan case: catalog records the pre-bc2b6fb
        // unsharded path, but the file was migrated to the sharded layout
        // and only the sharded key is populated.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        storage
            .put("markdown/04/04947b2f.md", b"sharded".to_vec())
            .await
            .unwrap();

        let got = resolve_markdown_key_verified(
            &storage,
            "markdown",
            "04947b2f",
            Some("markdown/04947b2f.md"),
        )
        .await;
        assert_eq!(got, "markdown/04/04947b2f.md");
    }

    #[tokio::test]
    async fn verified_returns_stored_when_sharded_is_absent() {
        // Pre-migration unsharded files: the stored path is the only real
        // location, even though the sharded derivation would be different.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        storage
            .put("markdown/legacy.md", b"flat".to_vec())
            .await
            .unwrap();

        let got = resolve_markdown_key_verified(
            &storage,
            "markdown",
            "legacy",
            Some("markdown/legacy.md"),
        )
        .await;
        assert_eq!(got, "markdown/legacy.md");
    }

    #[tokio::test]
    async fn verified_short_circuits_when_stored_equals_sharded() {
        // When stored_path matches the sharded derivation, no extra HEAD
        // should fire — the function returns the sharded key without
        // probing. We verify behavior (not the HEAD count) by seeding
        // only the sharded key; if the function probed stored first it
        // would still succeed, but the return value must be sharded.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        storage
            .put("markdown/ab/abcdef.md", b"hit".to_vec())
            .await
            .unwrap();

        let got = resolve_markdown_key_verified(
            &storage,
            "markdown",
            "abcdef",
            Some("markdown/ab/abcdef.md"),
        )
        .await;
        assert_eq!(got, "markdown/ab/abcdef.md");
    }

    #[tokio::test]
    async fn verified_falls_back_to_sharded_when_stored_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        // Seed nothing — we only check the resolution logic, not existence.
        let got = resolve_markdown_key_verified(&storage, "markdown", "abcdef", None).await;
        assert_eq!(got, "markdown/ab/abcdef.md");
    }

    #[tokio::test]
    async fn verified_returns_sharded_when_neither_exists() {
        // True orphan: catalog has a stale path, neither sharded nor stored
        // exists. Function returns the sharded key; callers then exists()
        // it, see false, and flag the orphan correctly.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let got =
            resolve_markdown_key_verified(&storage, "markdown", "gone", Some("markdown/gone.md"))
                .await;
        assert_eq!(got, "markdown/go/gone.md");
    }

    #[tokio::test]
    async fn list_and_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        // Seed a sharded layout: markdown/ab/abcdef.md and markdown/12/123456.md
        storage
            .put("markdown/ab/abcdef.md", b"hello abc".to_vec())
            .await
            .unwrap();
        storage
            .put("markdown/12/123456.md", b"hello 123".to_vec())
            .await
            .unwrap();
        // AppleDouble stub should be filtered out.
        storage
            .put("markdown/ab/._abcdef.md", b"junk".to_vec())
            .await
            .unwrap();
        // Non-md file in prefix should be ignored.
        storage
            .put("markdown/ab/README.txt", b"nope".to_vec())
            .await
            .unwrap();

        let mut stems = list_markdown_stems_via(&storage, "markdown").await.unwrap();
        stems.sort();
        assert_eq!(stems, vec!["123456".to_string(), "abcdef".to_string()]);

        let metas = list_markdown_meta_via(&storage, "markdown").await.unwrap();
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().any(|(s, m)| s == "abcdef" && m.size == 9));

        let doc = read_markdown_via(&storage, "markdown", "abcdef").await;
        assert_eq!(doc.as_deref(), Some("hello abc"));

        let missing = read_markdown_via(&storage, "markdown", "nope").await;
        assert!(missing.is_none());

        assert!(markdown_exists_via(&storage, "markdown", "abcdef")
            .await
            .unwrap());
        assert!(!markdown_exists_via(&storage, "markdown", "nope")
            .await
            .unwrap());
    }
}
