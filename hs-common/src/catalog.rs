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

    /// Recorded when the source bytes are not a valid PDF (or EPUB / HTML)
    /// at all — e.g. a paywall HTML renamed `.pdf`, truncated downloads,
    /// ransomware stubs. This is not a retry-able error: the *content* is
    /// wrong, so no number of reconvert attempts will succeed. `catalog_
    /// repair`'s `stuck_convert` direction skips rows with this stamp, and
    /// the scribe watch-events daemon writes it before acking NATS so the
    /// queue drains instead of re-processing the same dead files forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion_failed: Option<ConversionFailure>,

    // Embedding metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingMeta>,

    /// Recorded when the distill indexer chose to skip this document
    /// (empty markdown, zero chunks after quality filtering, etc). Lets
    /// `catalog_list` surface "stuck" documents and lets backfill avoid
    /// retrying known dead-ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_skip: Option<EmbeddingSkip>,

    /// Recorded when `catalog_repair` synthesized a row for an orphan file
    /// (PDF/HTML on disk with no prior catalog entry). Distinguishes
    /// repaired rows from rows produced by the normal download path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairMeta>,
}

/// A successful conversion. The presence of this struct on a catalog
/// entry means "markdown exists and is usable" — there is no `failed`
/// state. If a converter errors, the operator sees it in logs and no
/// `conversion` block is written. One path per feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionMeta {
    /// Descriptive name of the converter that produced this markdown:
    /// `"scribe-vlm"`, `"html-parser"`, `"epub-parser"`, or a
    /// `"catalog_repair:<direction>"` marker for synthesized repair rows.
    pub server: String,
    /// Wall-clock convert time. Stored as `f64` so sub-second conversions
    /// (tiny EPUBs / HTMLs) don't read as zero.
    pub duration_secs: f64,
    pub total_pages: u64,
    pub converted_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageOffset>,
}

/// Terminal convert failure — written when the source is unconvertable by
/// content, not by transient error. Presence of this stamp means "stop
/// trying; look at the reason and either re-download or quarantine."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionFailure {
    /// Short machine-readable reason. Canonical values:
    /// `"unsupported_content_type:html"` (paywall HTML saved as .pdf),
    /// `"unsupported_content_type:binary"` (random bytes, truncated
    /// download, encrypted PDF, etc.), `"quarantine_scan:html"` /
    /// `"quarantine_scan:binary"` (flagged by the offline quarantine
    /// sweeper).
    pub reason: String,
    /// RFC3339 timestamp of when this failure was recorded.
    pub at: String,
    /// Attempt counter. Always 1 today (terminal on first detection); the
    /// field is carried so a future retry policy has a place to increment.
    #[serde(default = "default_attempts")]
    pub attempts: u32,
}

fn default_attempts() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSkip {
    /// Short machine-readable token, e.g. "empty_markdown",
    /// "zero_chunks_after_quality_filter".
    pub reason: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairMeta {
    pub repaired_at: String,
    pub reason: String,
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
    duration_secs: f64,
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
/// Returns early when `chunks_indexed == 0` so nothing gets stamped unless
/// Qdrant actually received points.
pub fn update_embedding_catalog(
    catalog_dir: &Path,
    stem: &str,
    server: &str,
    chunks_indexed: u32,
    compute_device: &str,
) {
    if chunks_indexed == 0 {
        return;
    }
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
// Garage/S3 bucket with the same code. `prefix` is the sub-path inside the
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

#[cfg(feature = "storage")]
pub async fn delete_catalog_entry_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
) -> anyhow::Result<()> {
    let key = catalog_key(prefix, stem);
    storage.delete(&key).await
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

/// Default concurrency for the parallel catalog fetcher — one HTTP connection
/// per fetch, bounded against local Garage's typical inflight ceiling.
#[cfg(feature = "storage")]
pub const CATALOG_FETCH_CONCURRENCY: usize = 24;

/// Parallel variant of [`list_catalog_entries_via`] used by `catalog_repair`.
///
/// The serial version issues one awaited `storage.get` per YAML — fine for a
/// dozen rows, but at O(N) × single-flight latency it blows through the
/// MCP 4-minute client budget on a few-thousand-row catalog. This fetcher
/// spreads the per-key GETs across `concurrency` in-flight requests via
/// `futures::stream::buffer_unordered`, then re-slots them into their
/// `storage.list` order so callers observe the same sequence the serial
/// version would produce.
///
/// Error policy (divergent from `list_catalog_entries_via` by design):
/// - Per-key `storage.get` failure or 10s timeout → `Err` from the whole
///   fetcher, with the offending key in the message. `storage.list`
///   already promised this key exists, so a GET failure signals a real
///   storage-integrity problem worth surfacing loudly.
/// - Deserialization failure of a fetched YAML → silently skipped, matching
///   the existing lenient schema-evolution behavior.
///
/// `on_progress(done, total)` fires every 64 completed GETs and once at the
/// end. `total` is the post-filter YAML count (skips `._` hidden files and
/// non-`.yaml` extensions), not the raw list length.
#[cfg(feature = "storage")]
pub async fn list_catalog_entries_parallel(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    concurrency: usize,
    mut on_progress: impl FnMut(usize, usize),
) -> anyhow::Result<Vec<(String, crate::storage::ObjectMeta, CatalogEntry)>> {
    use futures::stream::{self, StreamExt};
    use std::time::Duration;

    let objects = storage.list(prefix).await?;
    let candidates: Vec<(usize, crate::storage::ObjectMeta, String)> = objects
        .into_iter()
        .enumerate()
        .filter_map(|(idx, obj)| {
            if !obj.key.ends_with(".yaml") {
                return None;
            }
            let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
            if filename.starts_with("._") {
                return None;
            }
            let stem = filename.trim_end_matches(".yaml").to_string();
            Some((idx, obj, stem))
        })
        .collect();

    let total = candidates.len();
    let mut slotted: Vec<Option<(String, crate::storage::ObjectMeta, CatalogEntry)>> =
        (0..total).map(|_| None).collect();
    let mut done: usize = 0;

    let mut stream = stream::iter(candidates.into_iter().enumerate().map(
        |(slot, (_original_idx, obj, stem))| {
            let key = obj.key.clone();
            async move {
                let res = tokio::time::timeout(Duration::from_secs(10), storage.get(&key)).await;
                (slot, obj, stem, key, res)
            }
        },
    ))
    .buffer_unordered(concurrency.max(1));

    while let Some((slot, obj, stem, key, res)) = stream.next().await {
        let bytes = match res {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                return Err(anyhow::anyhow!(
                    "list_catalog_entries_parallel: storage.get({key}) failed: {e}"
                ));
            }
            Err(_elapsed) => {
                return Err(anyhow::anyhow!(
                    "list_catalog_entries_parallel: storage.get({key}) timed out after 10s"
                ));
            }
        };
        done += 1;
        if done.is_multiple_of(64) || done == total {
            on_progress(done, total);
        }
        if let Ok(entry) = serde_yaml_ng::from_slice::<CatalogEntry>(&bytes) {
            slotted[slot] = Some((stem, obj, entry));
        }
    }

    Ok(slotted.into_iter().flatten().collect())
}

#[cfg(feature = "storage")]
#[allow(clippy::too_many_arguments)]
pub async fn update_conversion_catalog_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    server: &str,
    duration_secs: f64,
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

/// Stamp a terminal convert failure on the catalog row. Called by the
/// scribe watch-events daemon when the `/convert` endpoint rejects the
/// source bytes as unconvertable (e.g. HTML masquerading as PDF), and by
/// the `hs migrate quarantine-bad-content` sweeper when it relocates a
/// bad file. Idempotent: overwrites any prior stamp (the `attempts`
/// counter is preserved across stamps for future retry policies).
#[cfg(feature = "storage")]
pub async fn update_conversion_failed_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let mut entry = read_catalog_entry_via(storage, prefix, stem)
        .await
        .unwrap_or_default();
    let attempts = entry
        .conversion_failed
        .as_ref()
        .map(|f| f.attempts.saturating_add(1))
        .unwrap_or(1);
    entry.conversion_failed = Some(ConversionFailure {
        reason: reason.to_string(),
        at: chrono::Utc::now().to_rfc3339(),
        attempts,
    });
    write_catalog_entry_via(storage, prefix, stem, &entry).await
}

/// Record the outcome of an indexing attempt: stamp `embedding` when
/// Qdrant got points, otherwise stamp `embedding_skip`. Use this at the
/// integration seam (MCP, event_watch) so a 0-chunks return value is
/// always visible in the catalog rather than indistinguishable from
/// "never tried."
#[cfg(feature = "storage")]
pub async fn record_embedding_outcome_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    server: &str,
    chunks_indexed: u32,
    compute_device: &str,
) -> anyhow::Result<()> {
    if chunks_indexed > 0 {
        update_embedding_catalog_via(
            storage,
            prefix,
            stem,
            server,
            chunks_indexed,
            compute_device,
        )
        .await
    } else {
        update_embedding_skip_via(storage, prefix, stem, "zero_chunks_or_empty").await
    }
}

/// Stamp a skip on the embedding stage. Used by the distill pipeline when
/// it intentionally chooses not to index a document (empty markdown, zero
/// chunks after quality filtering). Without this, the absence of an
/// `embedding` block is indistinguishable from "never tried" and the doc
/// would be retried forever.
#[cfg(feature = "storage")]
pub async fn update_embedding_skip_via(
    storage: &dyn crate::storage::Storage,
    prefix: &str,
    stem: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let mut entry = read_catalog_entry_via(storage, prefix, stem)
        .await
        .unwrap_or_default();
    entry.embedding_skip = Some(EmbeddingSkip {
        reason: reason.to_string(),
        at: chrono::Utc::now().to_rfc3339(),
    });
    write_catalog_entry_via(storage, prefix, stem, &entry).await
}

/// Path-variant of `update_embedding_skip_via` for the local-CLI flows that
/// still walk the filesystem directly.
pub fn update_embedding_skip(catalog_dir: &Path, stem: &str, reason: &str) {
    let mut entry = read_catalog_entry(catalog_dir, stem).unwrap_or_default();
    entry.embedding_skip = Some(EmbeddingSkip {
        reason: reason.to_string(),
        at: chrono::Utc::now().to_rfc3339(),
    });
    write_catalog_entry(catalog_dir, stem, &entry);
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
    if chunks_indexed == 0 {
        return Ok(());
    }
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
    use crate::storage::{LocalFsStorage, Storage};

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

    #[tokio::test]
    async fn duration_secs_subsecond_roundtrip() {
        // A stub PDF that fails in 420 ms must round-trip through YAML as
        // exact f64, not truncate to 0.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            conversion: Some(ConversionMeta {
                server: "scribe-vlm".into(),
                duration_secs: 0.42,
                total_pages: 1,
                converted_at: "2026-04-15T19:50:02Z".into(),
                pages: vec![],
            }),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "stub", &entry)
            .await
            .unwrap();

        let got = read_catalog_entry_via(&storage, "catalog", "stub")
            .await
            .unwrap();
        let conv = got.conversion.unwrap();
        assert_eq!(conv.duration_secs, 0.42);
    }

    #[test]
    fn duration_secs_back_compat_integer_yaml() {
        // Pre-rc.246 catalog rows wrote duration_secs as an integer. YAML 1.2
        // scalar conversion should let those rows continue to deserialize into
        // the new f64 field — verify so the migration doesn't strand history.
        let yaml = r#"
conversion:
  server: scribe
  duration_secs: 5
  total_pages: 12
  converted_at: "2025-01-01T00:00:00Z"
"#;
        let entry: CatalogEntry =
            serde_yaml_ng::from_str(yaml).expect("integer duration_secs must still parse");
        let conv = entry.conversion.expect("conversion present");
        assert_eq!(conv.duration_secs, 5.0);
        assert_eq!(conv.total_pages, 12);
    }

    #[tokio::test]
    async fn conversion_failed_roundtrip_and_increments_attempts() {
        // Stamp, read back, stamp again — attempts counter should
        // increment so a future retry policy can see how many tries the
        // file has already eaten.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        update_conversion_failed_via(
            &storage,
            "catalog",
            "paywalled",
            "unsupported_content_type:html",
        )
        .await
        .unwrap();
        let entry = read_catalog_entry_via(&storage, "catalog", "paywalled")
            .await
            .expect("row stamped");
        let f1 = entry.conversion_failed.expect("field present");
        assert_eq!(f1.reason, "unsupported_content_type:html");
        assert_eq!(f1.attempts, 1);

        update_conversion_failed_via(
            &storage,
            "catalog",
            "paywalled",
            "unsupported_content_type:html",
        )
        .await
        .unwrap();
        let entry2 = read_catalog_entry_via(&storage, "catalog", "paywalled")
            .await
            .unwrap();
        assert_eq!(entry2.conversion_failed.unwrap().attempts, 2);
    }

    #[tokio::test]
    async fn parallel_fetcher_matches_serial() {
        // 100 valid catalog rows + 3 malformed YAML objects. Both fetchers
        // must return the same (stem -> title) set — the malformed ones are
        // silently skipped by both paths, and any other divergence is a bug.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        for i in 0..100 {
            let stem = format!("doc_{i:03}");
            let entry = CatalogEntry {
                title: Some(format!("Title {i}")),
                ..Default::default()
            };
            write_catalog_entry_via(&storage, "catalog", &stem, &entry)
                .await
                .unwrap();
        }
        for stem in ["bad_a", "bad_b", "bad_c"] {
            let key = format!("catalog/{}", crate::sharded_key(stem, "yaml"));
            storage
                .put(&key, b"{{ not: valid ::: yaml }}\n".to_vec())
                .await
                .unwrap();
        }

        let serial = list_catalog_entries_via(&storage, "catalog").await.unwrap();
        let parallel = list_catalog_entries_parallel(&storage, "catalog", 8, |_, _| {})
            .await
            .unwrap();

        use std::collections::BTreeMap;
        let serial_map: BTreeMap<String, Option<String>> = serial
            .into_iter()
            .map(|(stem, _meta, entry)| (stem, entry.title))
            .collect();
        let parallel_map: BTreeMap<String, Option<String>> = parallel
            .into_iter()
            .map(|(stem, _meta, entry)| (stem, entry.title))
            .collect();

        assert_eq!(serial_map.len(), 100, "100 valid rows, 3 malformed skipped");
        assert_eq!(parallel_map, serial_map);
    }

    #[tokio::test]
    async fn parallel_fetcher_fails_loud_on_get_error() {
        // A storage where one key's `get` returns `Err` must cause the
        // whole fetcher to fail — not skip the row silently — and the
        // offending key has to appear in the error so operators can
        // diagnose. Same match arm handles the 10s timeout branch; an
        // error-based mock keeps the test synchronous and deterministic.
        use crate::storage::{ObjectMeta, Storage};
        use async_trait::async_trait;
        use std::sync::Arc;

        struct FailOnKey {
            keys: Vec<String>,
            fail: String,
        }

        #[async_trait]
        impl Storage for FailOnKey {
            async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
                if key == self.fail {
                    return Err(anyhow::anyhow!("simulated backend 500"));
                }
                let title = key.trim_end_matches(".yaml").rsplit('/').next().unwrap();
                let entry = CatalogEntry {
                    title: Some(title.into()),
                    ..Default::default()
                };
                Ok(serde_yaml_ng::to_string(&entry)?.into_bytes())
            }
            async fn put(&self, _: &str, _: Vec<u8>) -> anyhow::Result<()> {
                unimplemented!()
            }
            async fn head(&self, _: &str) -> anyhow::Result<Option<ObjectMeta>> {
                unimplemented!()
            }
            async fn list(&self, _: &str) -> anyhow::Result<Vec<ObjectMeta>> {
                Ok(self
                    .keys
                    .iter()
                    .map(|k| ObjectMeta {
                        key: k.clone(),
                        size: 1,
                        last_modified: None,
                        etag: None,
                    })
                    .collect())
            }
            async fn delete(&self, _: &str) -> anyhow::Result<()> {
                unimplemented!()
            }
        }

        let keys: Vec<String> = (0..5).map(|i| format!("catalog/xx/doc_{i}.yaml")).collect();
        let fail = keys[2].clone();
        let storage = Arc::new(FailOnKey {
            keys,
            fail: fail.clone(),
        });

        let err = list_catalog_entries_parallel(&*storage, "catalog", 4, |_, _| {})
            .await
            .expect_err("must fail loud on GET error");
        let msg = err.to_string();
        assert!(
            msg.contains(&fail) && msg.contains("failed"),
            "error must name the offending key: {msg}"
        );
    }
}
