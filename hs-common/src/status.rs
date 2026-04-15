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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// "Download" | "Convert" | "Embed"
    pub activity: String,
    pub stem: String,
    pub name: String,
    /// Human detail: byte size, "12pg 45s", "27 chunks".
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
    pub duration_secs: Option<u64>,
    /// Embed metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<u32>,
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
            });
        }
        if let Some(ref conv) = entry.conversion {
            events.push(HistoryEvent {
                activity: "Convert".into(),
                stem: stem.clone(),
                name: name.clone(),
                detail: format!("{}pg {}s", conv.total_pages, conv.duration_secs),
                at: conv.converted_at.clone(),
                detail_bytes: None,
                pages: Some(conv.total_pages),
                duration_secs: Some(conv.duration_secs),
                chunks: None,
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
                });
            }
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
        embedded_documents,
        embedded_chunks,
    }
}
