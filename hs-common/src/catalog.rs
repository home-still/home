use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogEntry {
    // Paper metadata (from search/download)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<AuthorEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cited_by_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub download_urls: Vec<String>,

    // File references
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown_path: Option<String>,

    // Download metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

    // Conversion metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversion: Option<ConversionMeta>,

    // Embedding metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionMeta {
    pub server: String,
    pub duration_secs: u64,
    pub total_pages: u64,
    pub converted_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageOffset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingMeta {
    pub server: String,
    pub chunks_indexed: u32,
    pub compute_device: String,
    pub embedded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageOffset {
    pub page: usize,
    pub char_start: usize,
    pub char_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorEntry {
    pub name: String,
}

/// Compute page-to-character offsets from the final markdown.
/// Pages are separated by `\n\n---\n\n` in the joined output.
pub fn compute_page_offsets(markdown: &str) -> Vec<PageOffset> {
    let separator = "\n\n---\n\n";
    let mut offsets = Vec::new();
    let mut pos = 0;
    for (i, page) in markdown.split(separator).enumerate() {
        offsets.push(PageOffset {
            page: i + 1,
            char_start: pos,
            char_end: pos + page.len(),
        });
        pos += page.len() + separator.len();
    }
    offsets
}

/// Read an existing catalog entry, or return None if it doesn't exist.
pub fn read_catalog_entry(catalog_dir: &Path, stem: &str) -> Option<CatalogEntry> {
    let path = crate::sharded_path(catalog_dir, stem, "yaml");
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_yaml_ng::from_str(&contents).ok()
}

/// Write a catalog entry to disk (atomic write).
pub fn write_catalog_entry(catalog_dir: &Path, stem: &str, entry: &CatalogEntry) {
    let path = crate::sharded_path(catalog_dir, stem, "yaml");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(yaml) = serde_yaml_ng::to_string(entry) {
        let _ = std::fs::write(&path, yaml);
    }
}

/// Update only the conversion section of an existing catalog entry.
/// If no entry exists, creates a minimal one with just conversion metadata.
pub fn update_conversion_catalog(
    catalog_dir: &Path,
    stem: &str,
    server: &str,
    duration_secs: u64,
    total_pages: u64,
    pages: Vec<PageOffset>,
    markdown_path: &str,
) {
    let mut entry = read_catalog_entry(catalog_dir, stem).unwrap_or_default();

    entry.markdown_path = Some(markdown_path.to_string());
    entry.conversion = Some(ConversionMeta {
        server: server.to_string(),
        duration_secs,
        total_pages,
        converted_at: chrono::Utc::now().to_rfc3339(),
        pages,
    });

    write_catalog_entry(catalog_dir, stem, &entry);
}

/// Update only the embedding section of an existing catalog entry.
/// If no entry exists, creates a minimal one with just embedding metadata.
pub fn update_embedding_catalog(
    catalog_dir: &Path,
    stem: &str,
    server: &str,
    chunks_indexed: u32,
    compute_device: &str,
) {
    let mut entry = read_catalog_entry(catalog_dir, stem).unwrap_or_default();

    entry.embedding = Some(EmbeddingMeta {
        server: server.to_string(),
        chunks_indexed,
        compute_device: compute_device.to_string(),
        embedded_at: chrono::Utc::now().to_rfc3339(),
    });

    write_catalog_entry(catalog_dir, stem, &entry);
}

// ── Storage-backed variants ─────────────────────────────────────────────
//
// These mirror the path-based helpers above but read and write via the
// `Storage` trait, so callers can point at either a local filesystem or an
// S3/MinIO bucket with the same code. `prefix` is the sub-path inside the
// storage backend where catalog YAMLs live (e.g. "catalog" for a local
// backend rooted at the project dir, or "" for a dedicated `catalog` bucket).

#[cfg(feature = "storage")]
fn catalog_key(prefix: &str, stem: &str) -> String {
    let key = crate::sharded_key(stem, "yaml");
    if prefix.is_empty() {
        key
    } else {
        format!("{}/{}", prefix.trim_end_matches('/'), key)
    }
}

#[cfg(feature = "storage")]
pub async fn read_catalog_entry_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
) -> Option<CatalogEntry> {
    let key = catalog_key(prefix, stem);
    let bytes = storage.get(&key).await.ok()?;
    serde_yaml_ng::from_slice(&bytes).ok()
}

#[cfg(feature = "storage")]
pub async fn write_catalog_entry_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    entry: &CatalogEntry,
) -> anyhow::Result<()> {
    let key = catalog_key(prefix, stem);
    let yaml = serde_yaml_ng::to_string(entry)?;
    storage.put(&key, yaml.into_bytes()).await
}

/// List the stems of every catalog entry under `prefix`.
///
/// Walks `Storage::list(prefix)`, keeps keys ending in `.yaml`, strips the
/// sharded `XX/` directory, and returns the stem (filename without extension).
/// Works identically for `LocalFsStorage` (recursive fs walk) and `S3Storage`.
#[cfg(feature = "storage")]
pub async fn list_catalog_stems_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
) -> anyhow::Result<Vec<String>> {
    let objects = storage.list(prefix).await?;
    let mut stems = Vec::with_capacity(objects.len());
    for obj in objects {
        // Only yaml files are catalog entries.
        if !obj.key.ends_with(".yaml") {
            continue;
        }
        // Skip macOS AppleDouble metadata files just in case.
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        if filename.starts_with("._") {
            continue;
        }
        let stem = filename.trim_end_matches(".yaml").to_string();
        stems.push(stem);
    }
    Ok(stems)
}

/// List every catalog entry under `prefix`, deserialized.
///
/// Returns `(stem, ObjectMeta, CatalogEntry)` tuples so callers can preserve
/// filesystem mtime ordering (for history panes) without a second roundtrip.
/// Entries that fail to deserialize are silently skipped — matches the
/// lenient behavior of the pre-migration filesystem walk.
#[cfg(feature = "storage")]
pub async fn list_catalog_entries_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
) -> anyhow::Result<Vec<(String, crate::storage::ObjectMeta, CatalogEntry)>> {
    let objects = storage.list(prefix).await?;
    let mut out = Vec::with_capacity(objects.len());
    for obj in objects {
        if !obj.key.ends_with(".yaml") {
            continue;
        }
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        if filename.starts_with("._") {
            continue;
        }
        let stem = filename.trim_end_matches(".yaml").to_string();
        let Ok(bytes) = storage.get(&obj.key).await else {
            continue;
        };
        let Ok(entry) = serde_yaml_ng::from_slice::<CatalogEntry>(&bytes) else {
            continue;
        };
        out.push((stem, obj, entry));
    }
    Ok(out)
}

#[cfg(feature = "storage")]
#[allow(clippy::too_many_arguments)]
pub async fn update_conversion_catalog_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    server: &str,
    duration_secs: u64,
    total_pages: u64,
    pages: Vec<PageOffset>,
    markdown_path: &str,
) -> anyhow::Result<()> {
    let mut entry = read_catalog_entry_via(storage, prefix, stem)
        .await
        .unwrap_or_default();

    entry.markdown_path = Some(markdown_path.to_string());
    entry.conversion = Some(ConversionMeta {
        server: server.to_string(),
        duration_secs,
        total_pages,
        converted_at: chrono::Utc::now().to_rfc3339(),
        pages,
    });

    write_catalog_entry_via(storage, prefix, stem, &entry).await
}

#[cfg(feature = "storage")]
pub async fn update_embedding_catalog_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    server: &str,
    chunks_indexed: u32,
    compute_device: &str,
) -> anyhow::Result<()> {
    let mut entry = read_catalog_entry_via(storage, prefix, stem)
        .await
        .unwrap_or_default();

    entry.embedding = Some(EmbeddingMeta {
        server: server.to_string(),
        chunks_indexed,
        compute_device: compute_device.to_string(),
        embedded_at: chrono::Utc::now().to_rfc3339(),
    });

    write_catalog_entry_via(storage, prefix, stem, &entry).await
}

#[cfg(all(test, feature = "storage"))]
mod storage_tests {
    use super::*;
    use crate::storage::LocalFsStorage;

    #[tokio::test]
    async fn roundtrip_via_local_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            title: Some("Example".into()),
            sha256: Some("deadbeef".into()),
            ..Default::default()
        };

        write_catalog_entry_via(&storage, "catalog", "ab123", &entry)
            .await
            .unwrap();

        let got = read_catalog_entry_via(&storage, "catalog", "ab123")
            .await
            .unwrap();
        assert_eq!(got.title.as_deref(), Some("Example"));
        assert_eq!(got.sha256.as_deref(), Some("deadbeef"));

        // Empty prefix should also work (bucket-root layout)
        write_catalog_entry_via(&storage, "", "ab123", &entry)
            .await
            .unwrap();
        assert!(read_catalog_entry_via(&storage, "", "ab123")
            .await
            .is_some());
    }
}
