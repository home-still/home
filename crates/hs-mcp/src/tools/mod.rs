//! The server's tools, one file each.
//!
//! A tool's logic is a method on [`HomeStillMcp`](crate::HomeStillMcp) in
//! its own file, beside the arguments it takes. The `#[tool]` wrappers that
//! name and describe the tools are in `main.rs`, in the one router.

pub mod acquire;
pub mod capture;
pub mod cite;
pub mod discover;
pub mod documents;
pub mod graph;
pub mod personal_add;
pub mod read;
pub mod status;
pub mod synthesis_save;

use hs_common::catalog::CatalogEntry;
use hs_common::event_bus::specs::PAPERS_INGESTED;
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, StoreSpec};
use rmcp::model::{CallToolResult, Content};

use crate::HomeStillMcp;

/// The digits of the year a publication date starts with (`2021-03-04`).
const YEAR_DIGITS: usize = 4;

/// What a tool answers: its content, or its refusal as a result flagged
/// `isError`. A refusal is the tool's answer, not a protocol failure.
pub(crate) fn contents_result(outcome: Result<Vec<Content>, String>) -> CallToolResult {
    match outcome {
        Ok(content) => CallToolResult::success(content),
        Err(refusal) => CallToolResult::error(vec![Content::text(refusal)]),
    }
}

/// An object's bytes, or `None` when the storage holds no such object. Any
/// other failure is an error: an outage must not read as a missing object.
pub(crate) async fn object(storage: &dyn Storage, key: &str) -> Result<Option<Vec<u8>>, String> {
    match storage.get(key).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if hs_common::storage::is_not_found(&e) => Ok(None),
        Err(e) => Err(format!("reading {key} failed: {e:#}")),
    }
}

/// Whether the storage holds an object. An outage is an error, never
/// "absent".
pub(crate) async fn object_exists(storage: &dyn Storage, key: &str) -> Result<bool, String> {
    match storage.head(key).await {
        Ok(found) => Ok(found.is_some()),
        Err(e) if hs_common::storage::is_not_found(&e) => Ok(false),
        Err(e) => Err(format!("checking for {key} failed: {e:#}")),
    }
}

/// The extension of a stem's source object, probing the store's extensions
/// in order. `None` when the store has no such document.
pub(crate) async fn source_extension(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    stem: &str,
) -> Result<Option<&'static str>, String> {
    for extension in layout.source_extensions {
        if object_exists(storage, &layout.source_key(stem, extension)).await? {
            return Ok(Some(*extension));
        }
    }
    Ok(None)
}

/// A document's catalog entry, or `None` when it has none.
///
/// The personal store keeps the entry in a sidecar beside the document;
/// every other store keeps it under its catalog prefix.
pub(crate) async fn catalog_entry(
    storage: &dyn Storage,
    store: &StoreSpec,
    layout: &DocumentLayout,
    stem: &str,
) -> Result<Option<CatalogEntry>, String> {
    if store.name == hs_common::stores::PERSONAL {
        personal::services::catalog::read_card(storage, &layout.source_prefix, stem)
            .await
            .map_err(|e| e.to_string())
    } else {
        hs_common::catalog::read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
            .await
            .map_err(|e| format!("reading the catalog entry of {stem:?} failed: {e:#}"))
    }
}

/// The year a publication date (`2021-03-04`, `2021`) starts with.
pub(crate) fn publication_year(date: &str) -> Option<u16> {
    let year = date.get(..YEAR_DIGITS)?;
    if !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    year.parse().ok()
}

/// The names of a catalog entry's authors, in its order.
pub(crate) fn author_names(entry: &CatalogEntry) -> Vec<String> {
    entry
        .authors
        .iter()
        .map(|author| author.name.clone())
        .collect()
}

impl HomeStillMcp {
    /// Store `source` as a document, write its catalog entry and announce
    /// it on `papers.ingested`, so the pipeline that converts and indexes
    /// a downloaded paper converts and indexes this document.
    ///
    /// `entry` is what the caller knows about the document; when it was
    /// added, its size and its SHA-256 are filled in here. `announced_by`
    /// names the tool in the event. Answers the bytes stored.
    pub(crate) async fn store_document(
        &self,
        layout: &DocumentLayout,
        stem: &str,
        extension: &str,
        source: Vec<u8>,
        entry: CatalogEntry,
        announced_by: &str,
    ) -> Result<u64, String> {
        let storage: &dyn Storage = &*self.storage;
        let key = layout.source_key(stem, extension);
        let size_bytes = source.len() as u64;
        let sha256 = hs_common::word_boxes::source_sha256(&source);
        storage
            .put(&key, source)
            .await
            .map_err(|e| format!("storing {key} failed: {e:#}"))?;

        let entry = CatalogEntry {
            downloaded_at: Some(chrono::Utc::now().to_rfc3339()),
            file_size_bytes: Some(size_bytes),
            sha256: Some(sha256.clone()),
            ..entry
        };
        hs_common::catalog::write_catalog_entry_via(storage, &layout.catalog_prefix, stem, &entry)
            .await
            .map_err(|e| format!("writing the catalog entry of {stem:?} failed: {e:#}"))?;

        let event = hs_scribe::event_watch::IngestedEvent {
            key: key.clone(),
            sha256: Some(sha256),
            size_bytes: Some(size_bytes),
            source: Some(announced_by.to_string()),
        };
        let payload = serde_json::to_vec(&event).map_err(|e| e.to_string())?;
        self.events
            .publish(PAPERS_INGESTED.subject, &payload)
            .await
            .map_err(|e| {
                format!(
                    "the document is stored at {key} but announcing it failed, so it will not \
                     be converted and indexed until it is saved again: {e:#}"
                )
            })?;
        Ok(size_bytes)
    }
}
