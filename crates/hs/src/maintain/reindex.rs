//! `hs distill reindex`: index converted documents again, in place.
//!
//! The distill server writes a document's new records before it deletes the
//! earlier ones, so a document that is indexed again never leaves its store.

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use hs_common::catalog::CatalogEntry;
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use hs_distill::client::DistillClient;
use hs_distill::config::DistillClientConfig;

/// The `server` recorded on the embedding stamp of a document indexed again.
const STAMP_SERVER: &str = "reindex";

/// Documents between two progress lines.
const PROGRESS_EVERY_DOCS: usize = 25;

/// Which documents a run covers.
#[derive(Debug)]
pub(crate) enum Selection {
    /// The documents with these stems.
    Stems(Vec<String>),
    /// Every document with markdown. With `older_than`, the documents the
    /// catalog says were indexed at or after that time are left out, so a
    /// run that stopped is continued by naming the time it began.
    All { older_than: Option<DateTime<Utc>> },
}

/// What a run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub indexed: usize,
    pub chunks: u64,
    pub left_out_as_recent: usize,
    pub failed: usize,
}

enum Outcome {
    Indexed { chunks: u32 },
    Recent,
}

/// Whether the catalog says the document was indexed, or was found to hold
/// nothing to index, at or after `cutoff`. A stamp that records a failed
/// attempt, and one whose time cannot be read, do not count: such a
/// document is indexed again.
fn indexed_since(entry: &CatalogEntry, cutoff: DateTime<Utc>) -> bool {
    let embedded = entry.embedding.as_ref().map(|e| e.embedded_at.as_str());
    let skipped = entry
        .embedding_skip
        .as_ref()
        .filter(|skip| !hs_distill::reconcile::is_embed_failure(Some(&skip.reason)))
        .map(|skip| skip.at.as_str());
    [embedded, skipped]
        .into_iter()
        .flatten()
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .any(|at| at >= cutoff)
}

/// Index one document again and stamp its catalog entry. `listed_key` is
/// the markdown key a listing of the store returned; without it the key is
/// worked out from the stem and the catalog entry.
async fn reindex_one(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    store: &str,
    distill: &DistillClient,
    stem: &str,
    listed_key: Option<&str>,
    older_than: Option<DateTime<Utc>>,
) -> Result<Outcome> {
    hs_common::validate_stem(stem)
        .map_err(|e| anyhow::anyhow!("{stem:?} does not name a document: {e}"))?;
    let catalog = hs_common::catalog::read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
        .await
        .with_context(|| format!("reading the catalog entry of {stem}"))?;
    if let (Some(cutoff), Some(entry)) = (older_than, catalog.as_ref()) {
        if indexed_since(entry, cutoff) {
            return Ok(Outcome::Recent);
        }
    }
    let key = match listed_key {
        Some(key) => key.to_string(),
        None => hs_common::markdown::resolve_markdown_key_verified(
            storage,
            &layout.markdown_prefix,
            stem,
            catalog.as_ref().and_then(|e| e.markdown_path.as_deref()),
        )
        .await
        .with_context(|| format!("finding the markdown of {stem}"))?,
    };
    let result = distill
        .index_from_storage_with_catalog(store, storage, &key, catalog.as_ref())
        .await
        .with_context(|| format!("indexing {stem} from {key}"))?;
    hs_common::catalog::record_embedding_outcome_via(
        storage,
        &layout.catalog_prefix,
        stem,
        STAMP_SERVER,
        result.chunks_indexed,
        &result.embedding_device,
    )
    .await
    .with_context(|| {
        format!(
            "{stem} was indexed ({} chunks) but its catalog stamp was not written",
            result.chunks_indexed
        )
    })?;
    Ok(Outcome::Indexed {
        chunks: result.chunks_indexed,
    })
}

/// Index the selected documents of `store` again and stamp each one's
/// catalog entry, `concurrency` documents at once. One document's failure
/// does not stop the others: it is reported and counted, and the document
/// keeps the records and the stamp it had.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    store: &str,
    distill: &DistillClient,
    selection: Selection,
    concurrency: NonZeroUsize,
    reporter: &dyn Reporter,
) -> Result<Totals> {
    let (documents, older_than): (Vec<(String, Option<String>)>, _) = match selection {
        Selection::Stems(stems) => (stems.into_iter().map(|stem| (stem, None)).collect(), None),
        Selection::All { older_than } => {
            let listed =
                hs_common::markdown::list_markdown_meta_via(storage, &layout.markdown_prefix)
                    .await
                    .with_context(|| format!("listing the markdown of `{store}`"))?;
            let documents = listed
                .into_iter()
                .map(|(stem, object)| (stem, Some(object.key)))
                .collect();
            (documents, older_than)
        }
    };
    let total = documents.len();
    reporter.status("Reindex", &format!("{total} document(s) of `{store}`"));

    let mut outcomes = futures::stream::iter(documents.iter().map(|(stem, key)| async move {
        let outcome = reindex_one(
            storage,
            layout,
            store,
            distill,
            stem,
            key.as_deref(),
            older_than,
        )
        .await;
        (stem.as_str(), outcome)
    }))
    .buffer_unordered(concurrency.get());

    let mut totals = Totals::default();
    let mut done = 0usize;
    while let Some((stem, outcome)) = outcomes.next().await {
        done += 1;
        match outcome {
            Ok(Outcome::Indexed { chunks }) => {
                totals.indexed += 1;
                totals.chunks += u64::from(chunks);
            }
            Ok(Outcome::Recent) => totals.left_out_as_recent += 1,
            Err(e) => {
                totals.failed += 1;
                reporter.warn(&format!("{stem}: {e:#}"));
            }
        }
        if done.is_multiple_of(PROGRESS_EVERY_DOCS) || done == total {
            reporter.status(
                "Progress",
                &format!(
                    "{done}/{total}: indexed {} ({} chunks), left out as recent {}, failed {}",
                    totals.indexed, totals.chunks, totals.left_out_as_recent, totals.failed
                ),
            );
        }
    }
    Ok(totals)
}

/// `hs distill reindex`.
pub(crate) async fn cmd(
    stems: Vec<String>,
    all: bool,
    older_than: Option<String>,
    concurrency: NonZeroUsize,
    server_override: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let older_than = older_than
        .map(|time| {
            DateTime::parse_from_rfc3339(&time)
                .map(|t| t.with_timezone(&Utc))
                .with_context(|| {
                    format!(
                        "--older-than {time:?} is not an RFC 3339 time, such as 2026-01-31T00:00:00Z"
                    )
                })
        })
        .transpose()?;
    let selection = if all {
        Selection::All { older_than }
    } else {
        Selection::Stems(stems)
    };

    let cfg = DistillClientConfig::load().context("loading distill client config")?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let server_url = match server_override {
        Some(server) => server.to_string(),
        None => cfg.require_servers()?[0].clone(),
    };
    let distill = crate::distill_cmd::make_distill_client(&server_url)
        .await?
        .with_index_timeout(cfg.index_timeout());
    let stores = hs_common::stores::Stores::load().context("reading the store registry")?;
    let (spec, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let totals = run(
        storage.as_ref(),
        layout,
        spec.name,
        &distill,
        selection,
        concurrency,
        reporter.as_ref(),
    )
    .await?;
    if totals.failed > 0 {
        anyhow::bail!(
            "{} document(s) were not indexed again; {} were",
            totals.failed,
            totals.indexed
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{FakeServer, Response};
    use hs_common::catalog::{read_catalog_entry_via, write_catalog_entry_via, EmbeddingMeta};
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::{Stores, ACADEMIC, PERSONAL_PREFIX_DEFAULT};

    /// Chunks the stand-in server reports for every document it accepts.
    const CHUNKS: u32 = 3;

    fn stores() -> Stores {
        Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT).unwrap()
    }

    /// A distill server that answers every index request with `status`.
    async fn distill_answering(status: u16) -> FakeServer {
        FakeServer::start(move |req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/distill") => Response::json(
                status,
                &serde_json::json!({
                    "doc_id": "doc", "chunks_indexed": CHUNKS, "embedding_device": "cuda"
                }),
            ),
            _ => Response::json(404, &serde_json::json!({})),
        })
        .await
    }

    fn stamped(embedded_at: &str) -> CatalogEntry {
        CatalogEntry {
            embedding: Some(EmbeddingMeta {
                server: "earlier".into(),
                chunks_indexed: 1,
                compute_device: "cuda".into(),
                embedded_at: embedded_at.into(),
            }),
            ..Default::default()
        }
    }

    async fn stamp_server(storage: &LocalFsStorage, layout: &DocumentLayout, stem: &str) -> String {
        read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{stem} has no catalog entry"))
            .embedding
            .unwrap_or_else(|| panic!("{stem} has no embedding stamp"))
            .server
    }

    #[tokio::test]
    async fn a_whole_store_run_indexes_every_document_but_those_indexed_since_the_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        for (stem, embedded_at) in [
            ("stale", Some("2026-01-01T00:00:00+00:00")),
            ("fresh", Some("2026-07-01T00:00:00+00:00")),
            ("unstamped", None),
        ] {
            storage
                .put(&layout.markdown_key(stem), b"# Title\n\nBody.".to_vec())
                .await
                .unwrap();
            if let Some(at) = embedded_at {
                write_catalog_entry_via(&storage, &layout.catalog_prefix, stem, &stamped(at))
                    .await
                    .unwrap();
            }
        }
        let server = distill_answering(200).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let totals = run(
            &storage,
            layout,
            ACADEMIC,
            &distill,
            Selection::All {
                older_than: Some("2026-06-01T00:00:00Z".parse().unwrap()),
            },
            NonZeroUsize::MIN,
            &SilentReporter,
        )
        .await
        .unwrap();

        assert_eq!(
            totals,
            Totals {
                indexed: 2,
                chunks: u64::from(CHUNKS) * 2,
                left_out_as_recent: 1,
                failed: 0,
            }
        );
        assert_eq!(server.requests().len(), 2);
        assert_eq!(stamp_server(&storage, layout, "stale").await, STAMP_SERVER);
        assert_eq!(
            stamp_server(&storage, layout, "unstamped").await,
            STAMP_SERVER
        );
        assert_eq!(stamp_server(&storage, layout, "fresh").await, "earlier");
    }

    #[tokio::test]
    async fn a_document_without_markdown_is_counted_and_does_not_stop_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        storage
            .put(
                &layout.markdown_key("present"),
                b"# Title\n\nBody.".to_vec(),
            )
            .await
            .unwrap();
        let server = distill_answering(200).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let totals = run(
            &storage,
            layout,
            ACADEMIC,
            &distill,
            Selection::Stems(vec!["absent".into(), "present".into()]),
            NonZeroUsize::MIN,
            &SilentReporter,
        )
        .await
        .unwrap();

        assert_eq!(
            totals,
            Totals {
                indexed: 1,
                chunks: u64::from(CHUNKS),
                left_out_as_recent: 0,
                failed: 1,
            }
        );
        assert_eq!(
            stamp_server(&storage, layout, "present").await,
            STAMP_SERVER
        );
    }

    #[tokio::test]
    async fn a_document_the_server_refuses_keeps_the_stamp_it_had() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        storage
            .put(&layout.markdown_key("doc"), b"# Title\n\nBody.".to_vec())
            .await
            .unwrap();
        write_catalog_entry_via(
            &storage,
            &layout.catalog_prefix,
            "doc",
            &stamped("2026-01-01T00:00:00+00:00"),
        )
        .await
        .unwrap();
        let server = distill_answering(422).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let totals = run(
            &storage,
            layout,
            ACADEMIC,
            &distill,
            Selection::Stems(vec!["doc".into()]),
            NonZeroUsize::MIN,
            &SilentReporter,
        )
        .await
        .unwrap();

        assert_eq!(totals.failed, 1);
        assert_eq!(totals.indexed, 0);
        let row = read_catalog_entry_via(&storage, &layout.catalog_prefix, "doc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.embedding.unwrap().server, "earlier");
        assert!(
            row.embedding_skip.is_none(),
            "a document still in the store is not stamped as skipped"
        );
    }
}
