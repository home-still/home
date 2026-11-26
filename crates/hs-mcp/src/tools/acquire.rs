//! `acquire`: download a paper by DOI into the `academic` store.
//!
//! The download announces the paper on the event bus, and the pipeline that
//! follows (convert, index, word boxes) runs without this server. The answer
//! is therefore a state, not a document: `queued` for a paper that has just
//! been stored, `present` for one the store already held.

use hs_common::catalog::{AuthorEntry, CatalogEntry};
use rmcp::schemars;

use super::object_exists;
use crate::{to_json, HomeStillMcp};

/// The arguments of `acquire`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AcquireParams {
    #[schemars(description = "The paper's DOI, e.g. `10.1038/s41586-021-03819-2`.")]
    pub doi: String,
}

/// Whether `acquire` stored the paper in this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum AcquireState {
    /// Stored and announced in this call: conversion and indexing follow.
    Queued,
    /// The store already held the paper; nothing was downloaded.
    Present,
}

/// What `acquire` answers.
#[derive(Debug, serde::Serialize)]
struct Acquired {
    /// The store the paper is in: always `academic`.
    store: &'static str,
    doc_id: String,
    doi: String,
    state: AcquireState,
    /// Whether the paper's text is there to `read` and `cite`.
    converted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// What to call next.
    next: String,
}

impl HomeStillMcp {
    /// Download the paper a DOI names, write its catalog entry, and say
    /// what state that leaves it in.
    pub(crate) async fn acquire_paper(&self, p: AcquireParams) -> Result<String, String> {
        use paper::ports::download_service::DownloadService;

        let doi = paper::stem::normalize_doi(&p.doi).map_err(|e| format!("invalid DOI: {e}"))?;
        let stem = paper::stem::doi_stem(&doi).map_err(|e| format!("invalid DOI: {e}"))?;
        self.require_event_bus("acquire").await?;

        let result = self
            .downloader
            .download_by_doi(&doi)
            .await
            .map_err(|e| format!("the download of {doi} failed: {e}"))?;

        // A paper that was already stored is only "present" when its catalog
        // entry exists too: a retry after a failed catalog write must repair
        // the entry, not report success over a missing one.
        let has_entry = result.skipped
            && hs_common::catalog::read_catalog_entry_via(
                &*self.storage,
                &self.academic.catalog_prefix,
                &stem,
            )
            .await
            .map_err(|e| format!("reading the catalog entry of '{stem}' failed: {e:#}"))?
            .is_some();

        if !has_entry {
            // A provider that errors is an error, not "no metadata": the
            // paper is stored, but the entry must not be written as if
            // nothing was known about it.
            let found = self
                .providers
                .provider(&paper::cli::ProviderArg::All)
                .get_by_doi(&doi)
                .await
                .map_err(|e| {
                    format!(
                        "{doi} is stored at {} but looking up its metadata failed, so no \
                         catalog entry was written (call acquire again once the providers \
                         answer): {e}",
                        result.file_path.display()
                    )
                })?;
            let entry = CatalogEntry {
                title: found.as_ref().map(|p| p.title.clone()),
                authors: found
                    .as_ref()
                    .map(|p| {
                        p.authors
                            .iter()
                            .map(|a| AuthorEntry {
                                name: a.name.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                doi: Some(doi.clone()),
                publication_date: found
                    .as_ref()
                    .and_then(|p| p.publication_date.map(|d| d.to_string())),
                abstract_text: found.as_ref().and_then(|p| p.abstract_text.clone()),
                cited_by_count: found.as_ref().and_then(|p| p.cited_by_count),
                source: found.as_ref().map(|p| p.source.clone()),
                download_urls: found
                    .as_ref()
                    .map(|p| p.download_urls.clone())
                    .unwrap_or_default(),
                pdf_path: Some(result.file_path.display().to_string()),
                downloaded_at: Some(chrono::Utc::now().to_rfc3339()),
                file_size_bytes: Some(result.size_bytes),
                sha256: Some(result.sha256.clone()),
                ..CatalogEntry::default()
            };
            hs_common::catalog::write_catalog_entry_via(
                &*self.storage,
                &self.academic.catalog_prefix,
                &stem,
                &entry,
            )
            .await
            .map_err(|e| {
                format!(
                    "{doi} is stored at {} but writing its catalog entry failed (call acquire \
                     again to retry): {e:#}",
                    result.file_path.display()
                )
            })?;
        }

        let converted = object_exists(&*self.storage, &self.academic.markdown_key(&stem)).await?;
        let state = if result.skipped {
            AcquireState::Present
        } else {
            AcquireState::Queued
        };
        to_json(&Acquired {
            store: hs_common::stores::ACADEMIC,
            next: next_step(&stem, converted),
            doc_id: stem,
            doi,
            state,
            converted,
            size_bytes: (!result.skipped).then_some(result.size_bytes),
            sha256: (!result.skipped).then(|| result.sha256.clone()),
        })
    }
}

/// What the caller does after `acquire`: read the paper when its text is
/// there, otherwise ask `status` until it is.
fn next_step(doc_id: &str, converted: bool) -> String {
    if converted {
        format!("read {{\"doc_id\": \"{doc_id}\"}} for the text; cite its passages with cite")
    } else {
        format!(
            "conversion and indexing run in the background: ask status {{\"doc_id\": \
             \"{doc_id}\"}} until it reports converted, then read and cite"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconverted_paper_is_sent_to_status() {
        let next = next_step("10.1234_abc", false);
        assert!(
            next.contains("status {\"doc_id\": \"10.1234_abc\"}"),
            "{next}"
        );
    }

    #[test]
    fn a_converted_paper_is_sent_to_read() {
        let next = next_step("10.1234_abc", true);
        assert!(
            next.starts_with("read {\"doc_id\": \"10.1234_abc\"}"),
            "{next}"
        );
    }
}
