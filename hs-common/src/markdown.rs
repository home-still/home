//! Storage-backed access to converted markdown documents.
//!
//! Mirror of `crate::catalog`'s `*_via` helpers, but for `.md` payloads that
//! don't need deserialization. `prefix` is the sub-path inside the storage
//! backend where markdown files live (e.g. `"markdown"` under a project root,
//! or `""` for a dedicated bucket).

use crate::storage::{ObjectMeta, Storage};

fn markdown_key(prefix: &str, stem: &str) -> String {
    let key = crate::sharded_key(stem, "md");
    if prefix.is_empty() {
        key
    } else {
        format!("{}/{}", prefix.trim_end_matches('/'), key)
    }
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
