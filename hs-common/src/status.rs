//! Unified pipeline-status snapshot.
//!
//! Both `hs status` (CLI TUI) and the MCP `system_status` tool render from the
//! same `StatusSnapshot` so CLI users and LLM consumers see identical truth.
//! The CLI layers on its own TUI-only chrome (byte counts, watcher/indexer
//! daemon state); everything structural lives here.

use serde::{Deserialize, Serialize};

#[cfg(feature = "catalog")]
use crate::catalog::CatalogEntry;
#[cfg(feature = "storage")]
use crate::storage::Storage;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub pipeline: PipelineCounts,
    #[serde(default)]
    pub scribe_instances: Vec<ServiceInstance>,
    #[serde(default)]
    pub distill_instances: Vec<ServiceInstance>,
    #[serde(default)]
    pub qdrant: Option<QdrantInfo>,
    #[serde(default)]
    pub history: Vec<HistoryEvent>,
    #[serde(default)]
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineCounts {
    pub documents: u64,
    pub pdfs: u64,
    pub html_fallbacks: u64,
    pub markdown: u64,
    pub catalog_entries: u64,
    /// Catalog rows stamped `conversion.failed == true` (stub_document et al).
    /// These have a PDF in `papers/` and a YAML in `catalog/` but no markdown —
    /// they account for the Documents − Markdown gap in the TUI pipeline panel.
    #[serde(default)]
    pub conversion_failed: u64,
    #[serde(default)]
    pub embedded_documents: Option<u64>,
    #[serde(default)]
    pub embedded_chunks: Option<u64>,
}

/// Per-service health row. Same shape for scribe and distill; fields unused
/// by a given service are left empty/None.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceInstance {
    pub url: String,
    pub healthy: bool,
    #[serde(default)]
    pub version: String,
    /// Compute device reported by the server (e.g. "Cuda", "Cpu"); distill-only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compute_device: String,
    /// Embed model name; distill-only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub embed_model: String,
    /// Collection name; distill-only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub collection: String,
    /// Short human label: "idle", "3 converting", "1 embedding", "unhealthy".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// In-flight request count (conversions for scribe, embeddings for distill).
    #[serde(default)]
    pub in_flight: u64,
    /// Slot capacity info (scribe VLM slots).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots_available: Option<u64>,
}

/// Convenience rollup of the first healthy distill instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QdrantInfo {
    pub collection: String,
    pub compute_device: String,
    #[serde(default)]
    pub embed_model: String,
    #[serde(default)]
    pub qdrant_version: String,
    /// Qdrant endpoint URL reported by distill, e.g. `http://host:6334`.
    /// Empty when an older distill server doesn't yet expose it.
    #[serde(default)]
    pub qdrant_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// "Download" | "Convert" | "Embed" | "EmbedSkip"
    pub activity: String,
    pub stem: String,
    pub name: String,
    /// Human detail: byte size, "12pg 45.2s", "27 chunks", skip reason.
    pub detail: String,
    /// RFC3339 timestamp.
    pub at: String,
    /// Raw byte count for Download events (used by renderers that want custom
    /// formatting); None for other activities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_bytes: Option<u64>,
    /// Convert metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Embed metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<u32>,
    /// Convert / EmbedSkip outcome flags. Surfacing these prevents stub-PDF
    /// failures and zero-chunk skips from being indistinguishable from clean
    /// successes in the history pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Build the history event list from catalog entries, newest first.
#[cfg(feature = "catalog")]
pub fn build_history(entries: &[(String, CatalogEntry)], limit: usize) -> Vec<HistoryEvent> {
    let mut events: Vec<HistoryEvent> = Vec::new();
    for (stem, entry) in entries {
        let name = entry.title.clone().unwrap_or_else(|| stem.clone());
        if let Some(ref dl_at) = entry.downloaded_at {
            let detail = entry
                .file_size_bytes
                .map(|b| b.to_string())
                .unwrap_or_default();
            events.push(HistoryEvent {
                activity: "Download".into(),
                stem: stem.clone(),
                name: name.clone(),
                detail,
                at: dl_at.clone(),
                detail_bytes: entry.file_size_bytes,
                pages: None,
                duration_secs: None,
                chunks: None,
                failed: None,
                reason: None,
            });
        }
        if let Some(ref conv) = entry.conversion {
            let detail = if conv.failed {
                let reason = conv.reason.as_deref().unwrap_or("unknown");
                format!(
                    "FAILED: {reason} ({}pg {:.1}s)",
                    conv.total_pages, conv.duration_secs
                )
            } else {
                format!("{}pg {:.1}s", conv.total_pages, conv.duration_secs)
            };
            events.push(HistoryEvent {
                activity: "Convert".into(),
                stem: stem.clone(),
                name: name.clone(),
                detail,
                at: conv.converted_at.clone(),
                detail_bytes: None,
                pages: Some(conv.total_pages),
                duration_secs: Some(conv.duration_secs),
                chunks: None,
                failed: if conv.failed { Some(true) } else { None },
                reason: conv.reason.clone(),
            });
        }
        if let Some(ref emb) = entry.embedding {
            // Zero-chunk "embeddings" were stamped by an older pipeline when
            // nothing reached Qdrant; they only pollute history.
            if emb.chunks_indexed > 0 {
                events.push(HistoryEvent {
                    activity: "Embed".into(),
                    stem: stem.clone(),
                    name: name.clone(),
                    detail: format!("{} chunks", emb.chunks_indexed),
                    at: emb.embedded_at.clone(),
                    detail_bytes: None,
                    pages: None,
                    duration_secs: None,
                    chunks: Some(emb.chunks_indexed),
                    failed: None,
                    reason: None,
                });
            }
        }
        if let Some(ref skip) = entry.embedding_skip {
            // Surfaces "converted but not embedded" decisions (empty markdown,
            // zero chunks after quality filter) so they're visible in the
            // history pane instead of being silently absent.
            events.push(HistoryEvent {
                activity: "EmbedSkip".into(),
                stem: stem.clone(),
                name: name.clone(),
                detail: skip.reason.clone(),
                at: skip.at.clone(),
                detail_bytes: None,
                pages: None,
                duration_secs: None,
                chunks: None,
                failed: None,
                reason: Some(skip.reason.clone()),
            });
        }
    }
    events.sort_by(|a, b| b.at.cmp(&a.at));
    events.truncate(limit);
    events
}

/// Count objects under `prefix` whose filename ends with `.{ext}`.
#[cfg(feature = "storage")]
pub async fn count_ext_via(storage: &dyn Storage, prefix: &str, ext: &str) -> u64 {
    let suffix = format!(".{ext}");
    match storage.list(prefix).await {
        Ok(objs) => objs
            .iter()
            .filter(|o| {
                o.key.ends_with(&suffix)
                    && o.key
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| !name.starts_with("._"))
            })
            .count() as u64,
        Err(_) => 0,
    }
}

/// Find document files (PDFs and HTML fallbacks) under `papers_prefix`
/// that have no matching catalog YAML under `catalog_prefix`. Returns
/// `(stem, ext)` pairs sorted by stem so output is deterministic.
///
/// This is the primary diagnostic for the documents → catalog gap that
/// the self-test surfaces. The repair tool consumes this list to
/// synthesize minimal catalog rows for orphan files, restoring source-
/// of-truth alignment without re-downloading.
#[cfg(feature = "storage")]
pub async fn list_orphan_document_stems(
    storage: &dyn Storage,
    papers_prefix: &str,
    catalog_prefix: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let papers = storage.list(papers_prefix).await?;
    let catalog = storage.list(catalog_prefix).await?;

    use std::collections::HashSet;
    let known: HashSet<String> = catalog
        .iter()
        .filter_map(|o| {
            if !o.key.ends_with(".yaml") {
                return None;
            }
            let filename = o.key.rsplit('/').next()?;
            if filename.starts_with("._") {
                return None;
            }
            Some(filename.trim_end_matches(".yaml").to_string())
        })
        .collect();

    let mut orphans: Vec<(String, String)> = Vec::new();
    for obj in papers {
        let filename = match obj.key.rsplit('/').next() {
            Some(f) => f,
            None => continue,
        };
        if filename.starts_with("._") {
            continue;
        }
        let (stem, ext) = match filename.rsplit_once('.') {
            Some((s, e)) if e == "pdf" || e == "html" => (s.to_string(), e.to_string()),
            _ => continue,
        };
        if !known.contains(&stem) {
            orphans.push((stem, ext));
        }
    }
    orphans.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(orphans)
}

/// List stems of catalog rows that claim a successful conversion but whose
/// markdown object is missing from storage. These are the textbook F5-class
/// orphans: the row says "I was converted" but the payload isn't where the
/// pipeline wrote it.
///
/// Rows with `conversion.failed == true` are skipped — those are intentional
/// stub/failure stamps, not orphans. Rows with no `conversion` at all are
/// also skipped (they never claimed to have markdown in the first place).
#[cfg(all(feature = "storage", feature = "catalog"))]
pub async fn list_catalog_rows_without_markdown(
    storage: &dyn Storage,
    catalog_prefix: &str,
    markdown_prefix: &str,
) -> anyhow::Result<Vec<String>> {
    let triples = crate::catalog::list_catalog_entries_via(storage, catalog_prefix).await?;

    let markdown_objects = storage.list(markdown_prefix).await?;
    use std::collections::HashSet;
    let markdown_stems: HashSet<String> = markdown_objects
        .iter()
        .filter_map(|o| {
            if !o.key.ends_with(".md") {
                return None;
            }
            let filename = o.key.rsplit('/').next()?;
            if filename.starts_with("._") {
                return None;
            }
            Some(filename.trim_end_matches(".md").to_string())
        })
        .collect();

    let mut orphans: Vec<String> = triples
        .into_iter()
        .filter_map(|(stem, _meta, entry)| {
            let claims_converted = entry.conversion.as_ref().is_some_and(|c| !c.failed);
            if claims_converted && !markdown_stems.contains(&stem) {
                Some(stem)
            } else {
                None
            }
        })
        .collect();
    orphans.sort();
    Ok(orphans)
}

/// List stems of catalog rows with no backing file in storage — neither a
/// PDF/HTML under `papers_prefix` nor a markdown under `markdown_prefix`.
/// These are phantom rows (YAML that survived a paper deletion, or a stale
/// synthesis from a prior `catalog_repair` run on content that later
/// vanished). They inflate `catalog_entries` above `documents` in the
/// pipeline rollup without being reachable by any downstream stage.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub async fn list_catalog_rows_without_source(
    storage: &dyn Storage,
    papers_prefix: &str,
    catalog_prefix: &str,
    markdown_prefix: &str,
) -> anyhow::Result<Vec<String>> {
    let triples = crate::catalog::list_catalog_entries_via(storage, catalog_prefix).await?;
    let papers = storage.list(papers_prefix).await?;
    let markdown = storage.list(markdown_prefix).await?;

    use std::collections::HashSet;
    let paper_stems: HashSet<String> = papers
        .iter()
        .filter_map(|o| {
            let filename = o.key.rsplit('/').next()?;
            if filename.starts_with("._") {
                return None;
            }
            let (stem, ext) = filename.rsplit_once('.')?;
            if ext == "pdf" || ext == "html" {
                Some(stem.to_string())
            } else {
                None
            }
        })
        .collect();
    let md_stems: HashSet<String> = markdown
        .iter()
        .filter_map(|o| {
            if !o.key.ends_with(".md") {
                return None;
            }
            let filename = o.key.rsplit('/').next()?;
            if filename.starts_with("._") {
                return None;
            }
            Some(filename.trim_end_matches(".md").to_string())
        })
        .collect();

    let mut orphans: Vec<String> = triples
        .into_iter()
        .filter_map(|(stem, _meta, _entry)| {
            if paper_stems.contains(&stem) || md_stems.contains(&stem) {
                None
            } else {
                Some(stem)
            }
        })
        .collect();
    orphans.sort();
    Ok(orphans)
}

/// Pipeline counts via Storage. Same source of truth as MCP today.
#[cfg(feature = "storage")]
pub async fn collect_pipeline_counts(
    storage: &dyn Storage,
    papers_prefix: &str,
    markdown_prefix: &str,
    catalog_prefix: &str,
    embedded_documents: Option<u64>,
    embedded_chunks: Option<u64>,
) -> PipelineCounts {
    let pdfs = count_ext_via(storage, papers_prefix, "pdf").await;
    let html_fallbacks = count_ext_via(storage, papers_prefix, "html").await;
    let documents = pdfs + html_fallbacks;
    let markdown = count_ext_via(storage, markdown_prefix, "md").await;
    let catalog_entries = count_ext_via(storage, catalog_prefix, "yaml").await;
    PipelineCounts {
        documents,
        pdfs,
        html_fallbacks,
        markdown,
        catalog_entries,
        // Populated separately by the caller after it scans the catalog
        // bodies; `collect_pipeline_counts` only does cheap metadata listings.
        conversion_failed: 0,
        embedded_documents,
        embedded_chunks,
    }
}

#[cfg(all(test, feature = "catalog"))]
mod history_tests {
    use super::*;
    use crate::catalog::{CatalogEntry, ConversionMeta, EmbeddingMeta, EmbeddingSkip};

    #[test]
    fn surfaces_failed_convert_and_embed_skip() {
        // One entry has a failed (stub PDF) conversion and a downstream skip
        // stamp; the other is a clean download + convert + embed. The history
        // should expose both failure and skip distinctly, not silently drop them.
        let stub = CatalogEntry {
            title: Some("Stub".into()),
            downloaded_at: Some("2026-04-15T19:50:01Z".into()),
            conversion: Some(ConversionMeta {
                server: "scribe-1".into(),
                duration_secs: 0.42,
                total_pages: 1,
                converted_at: "2026-04-15T19:50:02Z".into(),
                pages: vec![],
                failed: true,
                reason: Some("stub_document".into()),
            }),
            embedding_skip: Some(EmbeddingSkip {
                reason: "zero_chunks_or_empty".into(),
                at: "2026-04-15T19:50:03Z".into(),
            }),
            ..Default::default()
        };
        let good = CatalogEntry {
            title: Some("Real".into()),
            downloaded_at: Some("2026-04-15T18:00:00Z".into()),
            conversion: Some(ConversionMeta {
                server: "scribe-1".into(),
                duration_secs: 12.5,
                total_pages: 33,
                converted_at: "2026-04-15T18:01:00Z".into(),
                pages: vec![],
                failed: false,
                reason: None,
            }),
            embedding: Some(EmbeddingMeta {
                server: "distill-1".into(),
                chunks_indexed: 33,
                compute_device: "Cuda".into(),
                embedded_at: "2026-04-15T18:02:00Z".into(),
            }),
            ..Default::default()
        };

        let entries = vec![("stub".to_string(), stub), ("real".to_string(), good)];
        let events = build_history(&entries, 100);

        // Expect 6 events: 2 downloads + 2 converts + 1 embed + 1 embed_skip.
        assert_eq!(events.len(), 6, "events: {events:#?}");

        // Sorted newest-first by timestamp.
        let activities: Vec<&str> = events.iter().map(|e| e.activity.as_str()).collect();
        assert_eq!(
            activities,
            vec![
                "EmbedSkip",
                "Convert",
                "Download",
                "Embed",
                "Convert",
                "Download"
            ]
        );

        // The failed convert must carry both the flag and a FAILED-prefixed detail.
        let stub_convert = events
            .iter()
            .find(|e| e.activity == "Convert" && e.stem == "stub")
            .expect("stub convert event present");
        assert_eq!(stub_convert.failed, Some(true));
        assert_eq!(stub_convert.reason.as_deref(), Some("stub_document"));
        assert!(
            stub_convert.detail.starts_with("FAILED:"),
            "detail: {}",
            stub_convert.detail
        );

        // Sub-second conversion duration must round-trip through f64 with .1 precision.
        assert!(
            stub_convert.detail.contains("0.4s"),
            "expected sub-second formatting in: {}",
            stub_convert.detail
        );

        // Clean convert has no failed flag (skip_serializing_if drops it from JSON).
        let real_convert = events
            .iter()
            .find(|e| e.activity == "Convert" && e.stem == "real")
            .unwrap();
        assert_eq!(real_convert.failed, None);
        assert!(real_convert.detail.contains("12.5s"));

        // EmbedSkip carries the reason as both `reason` and `detail`.
        let skip = events.iter().find(|e| e.activity == "EmbedSkip").unwrap();
        assert_eq!(skip.reason.as_deref(), Some("zero_chunks_or_empty"));
        assert_eq!(skip.detail, "zero_chunks_or_empty");
    }
}
