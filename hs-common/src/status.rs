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

/// Threshold for `pipeline_drift` above which the self-test fails. Small
/// residuals (documents vs markdown+failed+in_flight) are expected during
/// normal conversion activity; larger values indicate catalog flag drift.
pub const PIPELINE_DRIFT_THRESHOLD: u64 = 3;

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
    /// Total source documents: pdfs + htmls + epubs.
    pub documents: u64,
    pub pdfs: u64,
    pub htmls: u64,
    pub epubs: u64,
    pub markdown: u64,
    pub catalog_entries: u64,
    #[serde(default)]
    pub embedded_documents: Option<u64>,
    #[serde(default)]
    pub embedded_chunks: Option<u64>,
    /// Catalog entries stamped `embedding_skip` (e.g. zero-chunk HTML
    /// stubs). These count toward `markdown` but are intentionally not
    /// embeddable, so TUI progress percentages should exclude them from
    /// the denominator.
    #[serde(default)]
    pub embedding_skipped: Option<u64>,
    /// Unaccounted rows: `documents − markdown − in_flight`. Small
    /// residual (<= threshold) is expected due to stage-in-progress;
    /// larger values indicate a conversion error the operator needs to
    /// investigate via logs (no "failed" catalog rows exist in this
    /// pipeline — conversion either writes markdown + catalog or
    /// propagates an error).
    #[serde(default)]
    pub pipeline_drift: u64,
    /// Threshold the self-test uses when asserting `pipeline_drift`.
    #[serde(default)]
    pub pipeline_drift_threshold: u64,
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
    /// Reason string — populated for `EmbedSkip` events, unused otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Marks events whose timestamp came from a `catalog_repair` backfill
    /// sweep. Only emitted when `build_history` is called with
    /// `include_repaired = true` — `catalog_recent` uses the same flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
}

/// Build the history event list from catalog entries, newest first.
///
/// When `include_repaired` is `false` (the default for both `system_status`
/// and `catalog_recent`), rows whose timestamp is a `catalog_repair`
/// backfill fingerprint are suppressed so the feed shows organic activity.
/// When `true`, those rows reappear annotated with `repair: true`.
#[cfg(feature = "catalog")]
pub fn build_history(
    entries: &[(String, CatalogEntry)],
    limit: usize,
    include_repaired: bool,
) -> Vec<HistoryEvent> {
    let mut events: Vec<HistoryEvent> = Vec::new();
    for (stem, entry) in entries {
        let name = entry.title.clone().unwrap_or_else(|| stem.clone());
        let repair_at: Option<&str> = entry.repair.as_ref().map(|r| r.repaired_at.as_str());
        if let Some(ref dl_at) = entry.downloaded_at {
            // Fingerprint: `downloaded_at` stamped by a prior
            // `catalog_repair` flag_drift pass collapses hundreds of rows
            // onto one nanosecond. Skip by default; mirror `catalog_recent`.
            let is_repair = repair_at == Some(dl_at.as_str());
            if is_repair && !include_repaired {
                // skip
            } else {
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
                    reason: None,
                    repair: if is_repair { Some(true) } else { None },
                });
            }
        }
        if let Some(ref conv) = entry.conversion {
            // Synthetic Convert fingerprint: flag_drift backfill stamps
            // `server = "catalog_repair:flag_drift"` and shares the batch
            // `now()` with `repair.repaired_at`. Matching on both guards
            // against a real conversion finishing at the same instant as an
            // unrelated repair on another row.
            let is_repair = conv.server == "catalog_repair:flag_drift"
                && repair_at == Some(conv.converted_at.as_str());
            if is_repair && !include_repaired {
                continue;
            }
            let detail = format!("{}pg {:.1}s", conv.total_pages, conv.duration_secs);
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
                reason: None,
                repair: if is_repair { Some(true) } else { None },
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
                    reason: None,
                    repair: None,
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
                reason: Some(skip.reason.clone()),
                repair: None,
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

/// A single listing-and-deserialization pass over the three prefixes that
/// `catalog_repair`'s seven scan directions all probe. Built once per
/// `catalog_repair` invocation and lent to each scan by reference, so the
/// storage-side cost is `3 LISTs + N parallel GETs` (concurrency-bounded)
/// instead of the pre-fix `~15 LISTs + N serial GETs × 7` that exceeded
/// the MCP 4-minute client budget at 3k+ rows.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub struct RepairSnapshot {
    pub papers: Vec<crate::storage::ObjectMeta>,
    pub catalog: Vec<(String, crate::storage::ObjectMeta, CatalogEntry)>,
    pub markdown: Vec<crate::storage::ObjectMeta>,
}

/// Build a [`RepairSnapshot`] by listing the three prefixes concurrently
/// and fetching every catalog YAML in parallel via
/// [`crate::catalog::list_catalog_entries_parallel`]. The progress callback
/// fires during the catalog fetch (every 64 completions and once at the end)
/// so callers can reset the MCP client's tool-call timeout with heartbeats.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub async fn build_repair_snapshot(
    storage: &dyn Storage,
    papers_prefix: &str,
    catalog_prefix: &str,
    markdown_prefix: &str,
    concurrency: usize,
    on_catalog_fetch_progress: impl FnMut(usize, usize),
) -> anyhow::Result<RepairSnapshot> {
    let (papers, markdown, catalog) = tokio::try_join!(
        storage.list(papers_prefix),
        storage.list(markdown_prefix),
        crate::catalog::list_catalog_entries_parallel(
            storage,
            catalog_prefix,
            concurrency,
            on_catalog_fetch_progress,
        ),
    )?;
    Ok(RepairSnapshot {
        papers,
        catalog,
        markdown,
    })
}

/// Find document files (PDFs and HTML fallbacks) under `papers` that have
/// no matching catalog row. Returns `(stem, ext)` pairs sorted by stem so
/// output is deterministic.
///
/// This is the primary diagnostic for the documents → catalog gap that
/// the self-test surfaces. The repair tool consumes this list to
/// synthesize minimal catalog rows for orphan files, restoring source-
/// of-truth alignment without re-downloading.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_orphan_document_stems(
    papers: &[crate::storage::ObjectMeta],
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
) -> Vec<(String, String)> {
    use std::collections::HashSet;
    let known: HashSet<&str> = catalog.iter().map(|(stem, _, _)| stem.as_str()).collect();

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
        if !known.contains(stem.as_str()) {
            orphans.push((stem, ext));
        }
    }
    orphans.sort_by(|a, b| a.0.cmp(&b.0));
    orphans
}

/// List stems of catalog rows that claim a successful conversion but whose
/// markdown object is missing from storage. These are the textbook F5-class
/// orphans: the row says "I was converted" but the payload isn't where the
/// pipeline wrote it.
///
/// Rows with no `conversion` at all are skipped (they never claimed to have
/// markdown in the first place).
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_rows_without_markdown(
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
    markdown: &[crate::storage::ObjectMeta],
) -> Vec<String> {
    use std::collections::HashSet;
    let markdown_stems: HashSet<String> = markdown
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

    let mut orphans: Vec<String> = catalog
        .iter()
        .filter_map(|(stem, _meta, entry)| {
            if entry.conversion.is_some() && !markdown_stems.contains(stem) {
                Some(stem.clone())
            } else {
                None
            }
        })
        .collect();
    orphans.sort();
    orphans
}

/// A catalog row whose recorded `markdown_path` disagrees with storage —
/// either the recorded path doesn't exist or the `markdown_path` field is
/// absent while a matching-filename markdown object is present under the
/// prefix at some other key.
///
/// Discovered by `list_catalog_rows_with_md_path_drift`; consumed by
/// `catalog_repair`'s `md_path_drift` direction to rewrite the catalog
/// `markdown_path` to the real location.
///
/// This is the rc.241 ghost-orphan regression's permanent fix: the
/// `bc2b6fb` sharding migration moved files to `markdown/{XX}/{stem}.md`
/// but pre-existing catalog rows still record `markdown/<stem>.md` (or
/// nothing at all), so `resolve_markdown_key_verified` has to do an extra
/// HEAD on every probe. Rewriting the catalog eliminates the drift at rest.
#[cfg(all(feature = "storage", feature = "catalog"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdPathDrift {
    pub stem: String,
    /// Whatever `catalog_entry.markdown_path` currently says (empty string
    /// if `None`). Guaranteed ≠ `resolved_path`.
    pub stale_path: String,
    /// The key at which the markdown actually lives, discovered by listing
    /// the markdown prefix and matching filename = `{stem}.md`.
    pub resolved_path: String,
}

/// List catalog rows whose `markdown_path` field disagrees with the
/// markdown file's actual storage location.
///
/// Returns every catalog row where:
/// - A markdown object with filename `{stem}.md` exists somewhere under
///   `markdown_prefix` (tolerant of both sharded and legacy unsharded
///   layouts), AND
/// - The row's `markdown_path` is either absent, or points to a different
///   key than where the file actually lives.
///
/// Prefers the deepest (most path segments) discovered key as the canonical
/// target — after `bc2b6fb` the sharded key `{prefix}/{XX}/{stem}.md` wins
/// over a legacy unsharded `{prefix}/{stem}.md`. If multiple sharded copies
/// somehow coexist the first-listed one is chosen; that's a separate
/// duplication class and not this scan's concern.
///
/// Rows with no matching markdown anywhere under the prefix are skipped —
/// those belong to `list_catalog_rows_without_markdown`, which the
/// `catalog_no_markdown` direction handles by clearing the conversion
/// stamp.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_rows_with_md_path_drift(
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
    markdown: &[crate::storage::ObjectMeta],
) -> Vec<MdPathDrift> {
    use std::collections::HashMap;
    let mut md_by_filename: HashMap<String, String> = HashMap::new();
    for obj in markdown {
        if !obj.key.ends_with(".md") {
            continue;
        }
        let Some(filename) = obj.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        md_by_filename
            .entry(filename.to_string())
            .and_modify(|existing| {
                // Prefer the key with more path segments — sharded
                // `{prefix}/{XX}/{stem}.md` beats unsharded
                // `{prefix}/{stem}.md`. Same depth: keep whichever we
                // saw first (stable order from storage.list).
                if obj.key.matches('/').count() > existing.matches('/').count() {
                    *existing = obj.key.clone();
                }
            })
            .or_insert_with(|| obj.key.clone());
    }

    let mut drift = Vec::new();
    for (stem, _meta, entry) in catalog {
        let filename = format!("{stem}.md");
        let Some(resolved) = md_by_filename.get(&filename) else {
            continue;
        };
        let current = entry.markdown_path.as_deref().unwrap_or("");
        if current != resolved.as_str() {
            drift.push(MdPathDrift {
                stem: stem.clone(),
                stale_path: current.to_string(),
                resolved_path: resolved.clone(),
            });
        }
    }
    drift.sort_by(|a, b| a.stem.cmp(&b.stem));
    drift
}

/// A catalog row whose stage flags contradict what's actually in storage.
/// Discovered by `list_catalog_flag_drift`; consumed by `catalog_repair`'s
/// `flag_drift` direction to backfill missing stamps from the storage truth.
#[cfg(all(feature = "storage", feature = "catalog"))]
#[derive(Debug, Clone)]
pub struct FlagDriftRow {
    pub stem: String,
    /// Row has `conversion == None` but a markdown object exists — probably
    /// pre-dates the `conversion` block or was cleared mid-pipeline.
    pub conversion_missing_with_markdown: bool,
    /// Row has `downloaded_at == None` but a PDF/HTML object exists — catalog
    /// pre-dates `downloaded_at`, or a synthetic repair row was upgraded.
    pub download_stamp_missing_with_source: bool,
    /// `last_modified` of the source PDF/HTML object, if present. Lets the
    /// repair write a per-object `downloaded_at` drawn from storage truth
    /// instead of a shared batch `now()` — without this the whole repair
    /// pass collapses into one nanosecond in the activity feed.
    pub source_last_modified: Option<std::time::SystemTime>,
    /// `last_modified` of the markdown object, if present. Same purpose for
    /// the synthetic Convert stamp emitted when `conversion_missing_with_markdown`.
    pub markdown_last_modified: Option<std::time::SystemTime>,
}

/// List catalog rows whose stage flags are out of sync with storage.
///
/// Two drift cases — see [`FlagDriftRow`]. Callers can repair either side
/// without deleting data (unlike the phantom-purge direction). Skips rows
/// where neither drift condition is true (healthy row).
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_flag_drift(
    papers: &[crate::storage::ObjectMeta],
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
    markdown: &[crate::storage::ObjectMeta],
) -> Vec<FlagDriftRow> {
    use std::collections::HashMap;
    // Keep the `last_modified` per stem so the repair can emit per-object
    // timestamps. PDF wins over HTML when both exist, matching the preference
    // in `list_catalog_stuck_convert`.
    let mut paper_mtime_by_stem: HashMap<String, (String, Option<std::time::SystemTime>)> =
        HashMap::new();
    for o in papers.iter() {
        let Some(filename) = o.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        let Some((stem, ext)) = filename.rsplit_once('.') else {
            continue;
        };
        match ext {
            "pdf" => {
                paper_mtime_by_stem.insert(stem.to_string(), (ext.to_string(), o.last_modified));
            }
            "html" | "epub" => {
                paper_mtime_by_stem
                    .entry(stem.to_string())
                    .or_insert_with(|| (ext.to_string(), o.last_modified));
            }
            _ => {}
        }
    }
    let mut md_mtime_by_stem: HashMap<String, Option<std::time::SystemTime>> = HashMap::new();
    for o in markdown.iter() {
        if !o.key.ends_with(".md") {
            continue;
        }
        let Some(filename) = o.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        let stem = filename.trim_end_matches(".md").to_string();
        md_mtime_by_stem.insert(stem, o.last_modified);
    }

    let mut drift: Vec<FlagDriftRow> = Vec::new();
    for (stem, _meta, entry) in catalog {
        let md_hit = md_mtime_by_stem.get(stem).copied();
        let paper_hit = paper_mtime_by_stem.get(stem).cloned();
        let conversion_missing = entry.conversion.is_none() && md_hit.is_some();
        let download_missing = entry.downloaded_at.is_none() && paper_hit.is_some();
        if conversion_missing || download_missing {
            drift.push(FlagDriftRow {
                stem: stem.clone(),
                conversion_missing_with_markdown: conversion_missing,
                download_stamp_missing_with_source: download_missing,
                source_last_modified: paper_hit.and_then(|(_ext, mtime)| mtime),
                markdown_last_modified: md_hit.flatten(),
            });
        }
    }
    drift.sort_by(|a, b| a.stem.cmp(&b.stem));
    drift
}

/// A catalog row whose `downloaded_at` or synthetic-Convert `converted_at`
/// matches its `repair.repaired_at` to the nanosecond — the fingerprint of a
/// prior `catalog_repair` flag_drift pass that stamped every row with one
/// batch `now()`. Rewritable to the storage object's `last_modified` so the
/// activity feed recovers useful chronology.
#[cfg(all(feature = "storage", feature = "catalog"))]
#[derive(Debug, Clone)]
pub struct FlagDriftResyncCandidate {
    pub stem: String,
    /// `downloaded_at` currently equals `repair.repaired_at` and the source
    /// object has a usable `last_modified`.
    pub resync_download: Option<std::time::SystemTime>,
    /// Synthetic flag_drift Convert whose `converted_at` equals
    /// `repair.repaired_at`; replace with the markdown `last_modified`.
    pub resync_conversion: Option<std::time::SystemTime>,
}

/// List catalog rows whose timestamps still carry a prior `catalog_repair`
/// batch stamp (the "uniform nanosecond" pattern that poisons `catalog_recent`).
///
/// Fingerprint (both conditions must hold per field):
/// - `entry.repair.is_some()` AND
/// - For download: `entry.downloaded_at == entry.repair.repaired_at`.
/// - For conversion: `conv.server == "catalog_repair:flag_drift"` AND
///   `conv.converted_at == entry.repair.repaired_at`.
///
/// Only rows with a storage object that actually has a `last_modified` are
/// returned — otherwise there's nothing better to rewrite to and the resync
/// would be a no-op.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_flag_drift_resync_candidates(
    papers: &[crate::storage::ObjectMeta],
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
    markdown: &[crate::storage::ObjectMeta],
) -> Vec<FlagDriftResyncCandidate> {
    use std::collections::HashMap;
    let mut paper_mtime_by_stem: HashMap<String, Option<std::time::SystemTime>> = HashMap::new();
    for o in papers.iter() {
        let Some(filename) = o.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        let Some((stem, ext)) = filename.rsplit_once('.') else {
            continue;
        };
        match ext {
            "pdf" => {
                paper_mtime_by_stem.insert(stem.to_string(), o.last_modified);
            }
            "html" => {
                paper_mtime_by_stem
                    .entry(stem.to_string())
                    .or_insert(o.last_modified);
            }
            _ => {}
        }
    }
    let mut md_mtime_by_stem: HashMap<String, Option<std::time::SystemTime>> = HashMap::new();
    for o in markdown.iter() {
        if !o.key.ends_with(".md") {
            continue;
        }
        let Some(filename) = o.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        let stem = filename.trim_end_matches(".md").to_string();
        md_mtime_by_stem.insert(stem, o.last_modified);
    }

    let mut out: Vec<FlagDriftResyncCandidate> = Vec::new();
    for (stem, _meta, entry) in catalog {
        let Some(ref repair) = entry.repair else {
            continue;
        };
        let repair_at = repair.repaired_at.as_str();

        let resync_download = match entry.downloaded_at.as_deref() {
            Some(dl) if dl == repair_at => paper_mtime_by_stem.get(stem).copied().flatten(),
            _ => None,
        };
        let resync_conversion = match entry.conversion.as_ref() {
            Some(conv)
                if conv.server == "catalog_repair:flag_drift" && conv.converted_at == repair_at =>
            {
                md_mtime_by_stem.get(stem).copied().flatten()
            }
            _ => None,
        };
        if resync_download.is_some() || resync_conversion.is_some() {
            out.push(FlagDriftResyncCandidate {
                stem: stem.clone(),
                resync_download,
                resync_conversion,
            });
        }
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    out
}

/// A stuck-convert row: catalog has no `conversion` stamp, but a source
/// file (PDF or HTML) is present in storage. Surfaces the 2026-04-18
/// drift-gate failure mode: docs that went through convert → markdown →
/// embed, had their markdown deleted, and whose catalog flags were
/// cleared by `catalog_no_markdown` repair without the corresponding
/// Qdrant purge. After the rc.260 purge-also-on-clear fix the catalog
/// is consistent, but the source remains and needs re-conversion.
#[cfg(all(feature = "storage", feature = "catalog"))]
#[derive(Debug, Clone)]
pub struct StuckConvertRow {
    pub stem: String,
    /// `"pdf"` or `"html"` — tells the caller which scribe path to use.
    pub source_ext: String,
}

/// List stems whose catalog has no `conversion` block but whose source
/// file (PDF or HTML) is in storage. Candidates for `scribe_requeue_stuck`.
///
/// Not to be confused with [`list_catalog_rows_without_source`] (phantoms
/// with neither source nor markdown) or [`list_catalog_flag_drift`] (flags
/// missing but storage evidence present for an already-completed stage).
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_stuck_convert(
    papers: &[crate::storage::ObjectMeta],
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
) -> Vec<StuckConvertRow> {
    use std::collections::HashMap;
    // stem -> extension. PDF wins over HTML when both exist so we prefer
    // the richer source; the convert path handles either.
    let mut source_by_stem: HashMap<String, String> = HashMap::new();
    for o in papers.iter() {
        let Some(filename) = o.key.rsplit('/').next() else {
            continue;
        };
        if filename.starts_with("._") {
            continue;
        }
        let Some((stem, ext)) = filename.rsplit_once('.') else {
            continue;
        };
        match ext {
            "pdf" => {
                source_by_stem.insert(stem.to_string(), "pdf".to_string());
            }
            "html" => {
                source_by_stem
                    .entry(stem.to_string())
                    .or_insert_with(|| "html".to_string());
            }
            _ => {}
        }
    }

    let mut stuck: Vec<StuckConvertRow> = Vec::new();
    for (stem, _meta, entry) in catalog {
        if entry.conversion.is_some() {
            continue;
        }
        let Some(ext) = source_by_stem.get(stem) else {
            continue;
        };
        stuck.push(StuckConvertRow {
            stem: stem.clone(),
            source_ext: ext.clone(),
        });
    }
    stuck.sort_by(|a, b| a.stem.cmp(&b.stem));
    stuck
}

/// List stems of catalog rows with no backing file in storage — neither a
/// PDF/HTML under `papers_prefix` nor a markdown under `markdown_prefix`.
/// These are phantom rows (YAML that survived a paper deletion, or a stale
/// synthesis from a prior `catalog_repair` run on content that later
/// vanished). They inflate `catalog_entries` above `documents` in the
/// pipeline rollup without being reachable by any downstream stage.
#[cfg(all(feature = "storage", feature = "catalog"))]
pub fn list_catalog_rows_without_source(
    papers: &[crate::storage::ObjectMeta],
    catalog: &[(String, crate::storage::ObjectMeta, CatalogEntry)],
    markdown: &[crate::storage::ObjectMeta],
) -> Vec<String> {
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

    let mut orphans: Vec<String> = catalog
        .iter()
        .filter_map(|(stem, _meta, _entry)| {
            if paper_stems.contains(stem) || md_stems.contains(stem) {
                None
            } else {
                Some(stem.clone())
            }
        })
        .collect();
    orphans.sort();
    orphans
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
    embedding_skipped: Option<u64>,
) -> PipelineCounts {
    let pdfs = count_ext_via(storage, papers_prefix, "pdf").await;
    let htmls = count_ext_via(storage, papers_prefix, "html").await;
    let epubs = count_ext_via(storage, papers_prefix, "epub").await;
    let documents = pdfs + htmls + epubs;
    let markdown = count_ext_via(storage, markdown_prefix, "md").await;
    let catalog_entries = count_ext_via(storage, catalog_prefix, "yaml").await;
    PipelineCounts {
        documents,
        pdfs,
        htmls,
        epubs,
        markdown,
        catalog_entries,
        embedded_documents,
        embedded_chunks,
        embedding_skipped,
        // Computed by caller once it knows total in_flight.
        pipeline_drift: 0,
        pipeline_drift_threshold: PIPELINE_DRIFT_THRESHOLD,
    }
}

#[cfg(all(test, feature = "catalog"))]
mod history_tests {
    use super::*;
    use crate::catalog::{CatalogEntry, ConversionMeta, EmbeddingMeta, EmbeddingSkip};

    #[test]
    fn surfaces_convert_embed_and_embed_skip() {
        // Entry with a downstream skip stamp (converted but zero-chunk embed)
        // plus a clean download+convert+embed entry. History should surface
        // both convert rows, the embed, and the skip distinctly.
        let skipped = CatalogEntry {
            title: Some("Skipped".into()),
            downloaded_at: Some("2026-04-15T19:50:01Z".into()),
            conversion: Some(ConversionMeta {
                server: "scribe-vlm".into(),
                duration_secs: 0.42,
                total_pages: 1,
                converted_at: "2026-04-15T19:50:02Z".into(),
                pages: vec![],
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
                server: "scribe-vlm".into(),
                duration_secs: 12.5,
                total_pages: 33,
                converted_at: "2026-04-15T18:01:00Z".into(),
                pages: vec![],
            }),
            embedding: Some(EmbeddingMeta {
                server: "distill-1".into(),
                chunks_indexed: 33,
                compute_device: "Cuda".into(),
                embedded_at: "2026-04-15T18:02:00Z".into(),
            }),
            ..Default::default()
        };

        let entries = vec![("skipped".to_string(), skipped), ("real".to_string(), good)];
        let events = build_history(&entries, 100, false);

        // Expect 6 events: 2 downloads + 2 converts + 1 embed + 1 embed_skip.
        assert_eq!(events.len(), 6, "events: {events:#?}");

        // Sub-second conversion duration round-trips through f64 with .1 precision.
        let skipped_convert = events
            .iter()
            .find(|e| e.activity == "Convert" && e.stem == "skipped")
            .expect("skipped convert event present");
        assert!(
            skipped_convert.detail.contains("0.4s"),
            "expected sub-second formatting in: {}",
            skipped_convert.detail
        );

        let real_convert = events
            .iter()
            .find(|e| e.activity == "Convert" && e.stem == "real")
            .unwrap();
        assert!(real_convert.detail.contains("12.5s"));

        // EmbedSkip carries the reason as both `reason` and `detail`.
        let skip = events.iter().find(|e| e.activity == "EmbedSkip").unwrap();
        assert_eq!(skip.reason.as_deref(), Some("zero_chunks_or_empty"));
        assert_eq!(skip.detail, "zero_chunks_or_empty");
    }
}

#[cfg(all(test, feature = "storage", feature = "catalog"))]
mod stuck_convert_tests {
    use super::*;
    use crate::catalog::{write_catalog_entry_via, CatalogEntry, ConversionMeta};
    use crate::storage::LocalFsStorage;

    #[tokio::test]
    async fn surfaces_entry_with_pdf_but_no_conversion() {
        // Two catalog entries:
        //  1. `stuck` — downloaded, no `conversion` block, PDF on disk.
        //     This is the 2026-04-18 drift case the tool must surface.
        //  2. `converted` — has a `conversion` stamp. Must NOT be listed
        //     (a successful prior convert is not a requeue candidate).
        // A third fixture (`dangling`) has a catalog entry but NO source
        // on disk — that's the phantom direction's domain, not ours.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let stuck = CatalogEntry {
            downloaded_at: Some("2026-04-15T17:27:52Z".into()),
            pdf_path: Some("10/stuck.pdf".into()),
            conversion: None,
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "stuck", &stuck)
            .await
            .unwrap();
        storage
            .put("papers/st/stuck.pdf", b"fake pdf".to_vec())
            .await
            .unwrap();

        let converted = CatalogEntry {
            downloaded_at: Some("2026-04-15T16:00:00Z".into()),
            pdf_path: Some("10/converted.pdf".into()),
            conversion: Some(ConversionMeta {
                server: "scribe-vlm".into(),
                duration_secs: 10.0,
                total_pages: 5,
                converted_at: "2026-04-15T16:01:00Z".into(),
                pages: vec![],
            }),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "converted", &converted)
            .await
            .unwrap();
        storage
            .put("papers/co/converted.pdf", b"fake pdf 2".to_vec())
            .await
            .unwrap();

        let dangling = CatalogEntry {
            downloaded_at: Some("2026-04-15T15:00:00Z".into()),
            conversion: None,
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "dangling", &dangling)
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let stuck_rows = list_catalog_stuck_convert(&snap.papers, &snap.catalog);

        assert_eq!(stuck_rows.len(), 1, "got: {stuck_rows:?}");
        assert_eq!(stuck_rows[0].stem, "stuck");
        assert_eq!(stuck_rows[0].source_ext, "pdf");
    }

    #[tokio::test]
    async fn prefers_pdf_when_both_pdf_and_html_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            downloaded_at: Some("2026-04-15T12:00:00Z".into()),
            conversion: None,
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "both", &entry)
            .await
            .unwrap();
        // Seed both — PDF should win.
        storage
            .put("papers/bo/both.html", b"<html/>".to_vec())
            .await
            .unwrap();
        storage
            .put("papers/bo/both.pdf", b"fake".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let rows = list_catalog_stuck_convert(&snap.papers, &snap.catalog);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_ext, "pdf");
    }
}

#[cfg(all(test, feature = "storage", feature = "catalog"))]
mod md_path_drift_tests {
    use super::*;
    use crate::catalog::{write_catalog_entry_via, CatalogEntry};
    use crate::storage::LocalFsStorage;

    #[tokio::test]
    async fn detects_stale_unsharded_path_with_sharded_file() {
        // The rc.241 regression shape: catalog row records pre-bc2b6fb
        // `markdown/<stem>.md` but the file is at sharded
        // `markdown/{XX}/<stem>.md`. Must be reported as drift, with the
        // sharded key as the resolved_path.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            markdown_path: Some("markdown/04947b2f.md".into()),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "04947b2f", &entry)
            .await
            .unwrap();
        storage
            .put("markdown/04/04947b2f.md", b"sharded-real".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_rows_with_md_path_drift(&snap.catalog, &snap.markdown);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].stem, "04947b2f");
        assert_eq!(drift[0].stale_path, "markdown/04947b2f.md");
        assert_eq!(drift[0].resolved_path, "markdown/04/04947b2f.md");
    }

    #[tokio::test]
    async fn detects_missing_markdown_path_when_file_present() {
        // Row with no markdown_path at all, but a markdown file exists
        // under the prefix → drift. resolved_path is the physical key.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            markdown_path: None,
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "ghostpath", &entry)
            .await
            .unwrap();
        storage
            .put("markdown/gh/ghostpath.md", b"content".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_rows_with_md_path_drift(&snap.catalog, &snap.markdown);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].stale_path, "");
        assert_eq!(drift[0].resolved_path, "markdown/gh/ghostpath.md");
    }

    #[tokio::test]
    async fn healthy_row_is_not_flagged() {
        // markdown_path matches actual storage location → no drift.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            markdown_path: Some("markdown/ab/abcdef.md".into()),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "abcdef", &entry)
            .await
            .unwrap();
        storage
            .put("markdown/ab/abcdef.md", b"content".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_rows_with_md_path_drift(&snap.catalog, &snap.markdown);
        assert!(drift.is_empty());
    }

    #[tokio::test]
    async fn skips_rows_with_no_markdown_file_anywhere() {
        // Row whose markdown is truly missing → not this scan's job. The
        // catalog_no_markdown direction handles these by clearing the
        // conversion stamp; flagging them here would double-repair.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            markdown_path: Some("markdown/gone.md".into()),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "gone", &entry)
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_rows_with_md_path_drift(&snap.catalog, &snap.markdown);
        assert!(drift.is_empty());
    }

    #[tokio::test]
    async fn prefers_sharded_when_both_copies_exist() {
        // Duplicate markdown (sharded + unsharded) — catalog should be
        // rewritten to the sharded canonical location.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            markdown_path: Some("markdown/duped.md".into()),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "duped", &entry)
            .await
            .unwrap();
        storage
            .put("markdown/duped.md", b"flat".to_vec())
            .await
            .unwrap();
        storage
            .put("markdown/du/duped.md", b"sharded".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_rows_with_md_path_drift(&snap.catalog, &snap.markdown);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].resolved_path, "markdown/du/duped.md");
    }
}

#[cfg(all(test, feature = "storage", feature = "catalog"))]
mod flag_drift_tests {
    use super::*;
    use crate::catalog::{write_catalog_entry_via, CatalogEntry, ConversionMeta, RepairMeta};
    use crate::storage::LocalFsStorage;

    #[tokio::test]
    async fn populates_source_last_modified_from_storage_mtime() {
        // A catalog row with no `downloaded_at` but a PDF on disk → drift. The
        // scanner must carry the PDF's `last_modified` through so the repair
        // can stamp a per-object timestamp instead of a batch `now()`.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            downloaded_at: None,
            conversion: None,
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "mtimed", &entry)
            .await
            .unwrap();
        storage
            .put("papers/mt/mtimed.pdf", b"fake pdf".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let drift = list_catalog_flag_drift(&snap.papers, &snap.catalog, &snap.markdown);
        assert_eq!(drift.len(), 1);
        let row = &drift[0];
        assert_eq!(row.stem, "mtimed");
        assert!(row.download_stamp_missing_with_source);
        assert!(
            row.source_last_modified.is_some(),
            "scanner must thread the source object's last_modified through"
        );
    }

    #[tokio::test]
    async fn resync_candidate_detects_batch_stamp_fingerprint() {
        // Fingerprint: `downloaded_at == repair.repaired_at`. The scanner must
        // return the stem only when the source object also has a `last_modified`
        // worth rewriting to — otherwise the resync would be a no-op.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let batch_stamp = "2026-04-20T15:09:23.905851118+00:00";
        let entry = CatalogEntry {
            downloaded_at: Some(batch_stamp.into()),
            repair: Some(RepairMeta {
                repaired_at: batch_stamp.into(),
                reason: "flag_drift backfill".into(),
            }),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "resyncable", &entry)
            .await
            .unwrap();
        storage
            .put("papers/re/resyncable.pdf", b"fake".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let candidates =
            list_catalog_flag_drift_resync_candidates(&snap.papers, &snap.catalog, &snap.markdown);
        assert_eq!(candidates.len(), 1);
        let cand = &candidates[0];
        assert_eq!(cand.stem, "resyncable");
        assert!(cand.resync_download.is_some());
        assert!(cand.resync_conversion.is_none());
    }

    #[tokio::test]
    async fn resync_candidate_skips_organic_rows() {
        // A row with `repair.is_some()` but `downloaded_at` DIFFERENT from
        // `repair.repaired_at` is organic — someone downloaded a paper after a
        // prior repair stamped the row. Must NOT be reported as resyncable.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let entry = CatalogEntry {
            downloaded_at: Some("2026-04-19T10:00:00+00:00".into()),
            repair: Some(RepairMeta {
                repaired_at: "2026-04-20T15:09:23.905851118+00:00".into(),
                reason: "flag_drift backfill".into(),
            }),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "organic", &entry)
            .await
            .unwrap();
        storage
            .put("papers/or/organic.pdf", b"fake".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let candidates =
            list_catalog_flag_drift_resync_candidates(&snap.papers, &snap.catalog, &snap.markdown);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn resync_candidate_detects_synthetic_conversion() {
        // Synthetic Convert fingerprint: `server == "catalog_repair:flag_drift"`
        // AND `converted_at == repair.repaired_at`. Must be reported with
        // `resync_conversion` populated from the markdown mtime.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        let batch_stamp = "2026-04-20T15:09:23.905851118+00:00";
        let entry = CatalogEntry {
            conversion: Some(ConversionMeta {
                server: "catalog_repair:flag_drift".into(),
                duration_secs: 0.0,
                total_pages: 0,
                converted_at: batch_stamp.into(),
                pages: vec![],
            }),
            repair: Some(RepairMeta {
                repaired_at: batch_stamp.into(),
                reason: "flag_drift backfill".into(),
            }),
            ..Default::default()
        };
        write_catalog_entry_via(&storage, "catalog", "synthconv", &entry)
            .await
            .unwrap();
        storage
            .put("markdown/sy/synthconv.md", b"md body".to_vec())
            .await
            .unwrap();

        let snap = build_repair_snapshot(&storage, "papers", "catalog", "markdown", 8, |_, _| {})
            .await
            .unwrap();
        let candidates =
            list_catalog_flag_drift_resync_candidates(&snap.papers, &snap.catalog, &snap.markdown);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].resync_conversion.is_some());
        assert!(candidates[0].resync_download.is_none());
    }
}
