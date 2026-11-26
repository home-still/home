//! `hs distill backfill`: index the documents that were converted and never
//! embedded.
//!
//! A document is a candidate when its catalog entry has a conversion stamp,
//! and no embedding stamp with chunks, and no record that indexing found
//! nothing to index. With `--dry-run` the command only counts them.

use anyhow::{Context, Result};
use hs_common::catalog::CatalogEntry;
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use hs_distill::client::DistillClient;
use serde::Serialize;

/// The `server` recorded on the embedding stamp of a document this command
/// indexed.
const STAMP_SERVER: &str = "backfill";

/// Stems shown as examples in the report.
const SAMPLE_LIMIT: usize = 10;

/// Documents between two progress lines.
const PROGRESS_EVERY_DOCS: usize = 25;

/// What a run does with the candidates.
pub(crate) enum Mode<'a> {
    /// Count the candidates; contact no server.
    DryRun,
    /// Index the candidates through `distill` into `store`.
    Apply {
        distill: &'a DistillClient,
        store: &'a str,
    },
}

/// The report of a run with `--dry-run`.
#[derive(Debug, Serialize)]
pub(crate) struct DryRunReport {
    pub candidates: usize,
    pub would_index: usize,
    pub samples: Vec<String>,
    pub dry_run: bool,
}

/// The report of a run without `--dry-run`.
#[derive(Debug, Serialize)]
pub(crate) struct AppliedReport {
    pub candidates: usize,
    pub indexed: u64,
    pub still_skipped: u64,
    pub errors: Vec<String>,
    pub samples: Vec<String>,
    pub dry_run: bool,
}

/// What a run found or did.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Report {
    DryRun(DryRunReport),
    Applied(AppliedReport),
}

/// Whether the catalog says the document was converted and has no indexed
/// chunks. A document whose last indexing found nothing to index is left out
/// unless `retry_skipped`.
fn is_candidate(entry: &CatalogEntry, retry_skipped: bool) -> bool {
    let embedded = entry
        .embedding
        .as_ref()
        .is_some_and(|e| e.chunks_indexed > 0);
    entry.conversion.is_some() && !embedded && (entry.embedding_skip.is_none() || retry_skipped)
}

/// What indexing one document came to: the chunks written, and the stamp
/// error when the document was indexed but its catalog entry was not
/// updated.
struct Indexed {
    chunks: u32,
    stamp_error: Option<String>,
}

/// Index the markdown of `stem` and stamp its catalog entry.
async fn index_one(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    distill: &DistillClient,
    store: &str,
    stem: &str,
) -> Result<Indexed> {
    let catalog = hs_common::catalog::read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
        .await
        .context("reading the catalog entry")?;
    let key = hs_common::markdown::resolve_markdown_key_verified(
        storage,
        &layout.markdown_prefix,
        stem,
        catalog.as_ref().and_then(|e| e.markdown_path.as_deref()),
    )
    .await
    .context("finding the markdown")?;
    if !storage
        .exists(&key)
        .await
        .with_context(|| format!("checking for {key}"))?
    {
        anyhow::bail!("Markdown not found at storage key '{key}'. Convert the source first.");
    }
    let result = distill
        .index_from_storage_with_catalog(store, storage, &key, catalog.as_ref())
        .await
        .with_context(|| format!("indexing {key}"))?;
    let stamp_error = hs_common::catalog::record_embedding_outcome_via(
        storage,
        &layout.catalog_prefix,
        stem,
        STAMP_SERVER,
        result.chunks_indexed,
        &result.embedding_device,
    )
    .await
    .err()
    .map(|e| {
        format!(
            "indexed {} chunks but the catalog stamp was not written: {e:#}",
            result.chunks_indexed
        )
    });
    Ok(Indexed {
        chunks: result.chunks_indexed,
        stamp_error,
    })
}

/// Find the candidates of the store `layout` describes and, in
/// [`Mode::Apply`], index the first `limit` of them one at a time. One
/// document's failure does not stop the others: it is reported and listed in
/// `errors`.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    mode: Mode<'_>,
    limit: Option<usize>,
    retry_skipped: bool,
    reporter: &dyn Reporter,
) -> Result<Report> {
    let entries = hs_common::catalog::list_catalog_entries_parallel(
        storage,
        &layout.catalog_prefix,
        hs_common::catalog::CATALOG_FETCH_CONCURRENCY,
        |_, _| {},
    )
    .await
    .context("listing the catalog")?;

    let mut candidates: Vec<String> = entries
        .into_iter()
        .filter(|(_, _, entry)| is_candidate(entry, retry_skipped))
        .map(|(stem, _, _)| stem)
        .collect();
    candidates.sort();
    let total = candidates.len();
    let selected: Vec<String> = candidates
        .into_iter()
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    let samples: Vec<String> = selected.iter().take(SAMPLE_LIMIT).cloned().collect();

    let (distill, store) = match mode {
        Mode::DryRun => {
            return Ok(Report::DryRun(DryRunReport {
                candidates: total,
                would_index: selected.len(),
                samples,
                dry_run: true,
            }));
        }
        Mode::Apply { distill, store } => (distill, store),
    };

    let selected_count = selected.len();
    let mut indexed = 0u64;
    let mut still_skipped = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for (done, stem) in selected.iter().enumerate() {
        match index_one(storage, layout, distill, store, stem).await {
            Ok(outcome) => {
                if let Some(message) = outcome.stamp_error {
                    let message = format!("{stem}: {message}");
                    reporter.warn(&message);
                    errors.push(message);
                }
                if outcome.chunks > 0 {
                    indexed += 1;
                } else {
                    still_skipped += 1;
                }
            }
            Err(e) => {
                let message = format!("{stem}: {e:#}");
                reporter.warn(&message);
                errors.push(message);
            }
        }
        let finished = done + 1;
        if finished.is_multiple_of(PROGRESS_EVERY_DOCS) || finished == selected_count {
            reporter.status(
                "Backfill",
                &format!(
                    "{finished}/{selected_count}: indexed {indexed}, still skipped {still_skipped}, errors {}",
                    errors.len()
                ),
            );
        }
    }

    Ok(Report::Applied(AppliedReport {
        candidates: total,
        indexed,
        still_skipped,
        errors,
        samples,
        dry_run: false,
    }))
}

/// `hs distill backfill`.
pub(crate) async fn cmd(
    dry_run: bool,
    limit: Option<usize>,
    retry_skipped: bool,
    server: Option<&str>,
    reporter: &dyn Reporter,
) -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (spec, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let report = if !dry_run {
        let distill = ctx.distill(server).await?;
        run(
            storage.as_ref(),
            layout,
            Mode::Apply {
                distill: &distill,
                store: spec.name,
            },
            limit,
            retry_skipped,
            reporter,
        )
        .await?
    } else {
        run(
            storage.as_ref(),
            layout,
            Mode::DryRun,
            limit,
            retry_skipped,
            reporter,
        )
        .await?
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    if let Report::Applied(applied) = &report {
        if !applied.errors.is_empty() {
            anyhow::bail!(
                "{} documents could not be indexed; {} were",
                applied.errors.len(),
                applied.indexed
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::{conversion, stores};
    use crate::test_http::{FakeServer, Response};
    use hs_common::catalog::{
        read_catalog_entry_via, write_catalog_entry_via, EmbeddingMeta, EmbeddingSkip,
    };
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;

    /// Chunks the stand-in server reports for every document it accepts.
    const CHUNKS: u32 = 4;

    /// A distill server that answers every index request with `chunks`
    /// chunks, or with an error when `fail` is set.
    async fn distill_answering(chunks: u32, fail: bool) -> FakeServer {
        FakeServer::start(move |req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/distill") if fail => {
                Response::json(500, &serde_json::json!({"error": "embedder down"}))
            }
            ("POST", "/distill") => Response::json(
                200,
                &serde_json::json!({
                    "doc_id": "doc", "chunks_indexed": chunks, "embedding_device": "cuda"
                }),
            ),
            _ => Response::json(404, &serde_json::json!({})),
        })
        .await
    }

    fn converted() -> CatalogEntry {
        CatalogEntry {
            conversion: Some(conversion()),
            ..Default::default()
        }
    }

    async fn seed(
        storage: &LocalFsStorage,
        layout: &DocumentLayout,
        stem: &str,
        entry: CatalogEntry,
        with_markdown: bool,
    ) {
        write_catalog_entry_via(storage, &layout.catalog_prefix, stem, &entry)
            .await
            .unwrap();
        if with_markdown {
            storage
                .put(&layout.markdown_key(stem), b"# Title\n\nBody.".to_vec())
                .await
                .unwrap();
        }
    }

    /// Only a converted document with no indexed chunks is a candidate, and
    /// a document whose indexing found nothing is one only on request.
    #[tokio::test]
    async fn the_candidates_are_converted_documents_with_no_indexed_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        seed(&storage, layout, "todo", converted(), true).await;
        seed(
            &storage,
            layout,
            "done",
            CatalogEntry {
                embedding: Some(EmbeddingMeta {
                    server: "earlier".into(),
                    chunks_indexed: 2,
                    compute_device: "cuda".into(),
                    embedded_at: "2026-01-01T00:00:00+00:00".into(),
                }),
                ..converted()
            },
            true,
        )
        .await;
        seed(
            &storage,
            layout,
            "skipped",
            CatalogEntry {
                embedding_skip: Some(EmbeddingSkip {
                    reason: "empty_markdown".into(),
                    at: "2026-01-01T00:00:00+00:00".into(),
                }),
                ..converted()
            },
            true,
        )
        .await;
        seed(
            &storage,
            layout,
            "unconverted",
            CatalogEntry::default(),
            false,
        )
        .await;

        let plain = run(&storage, layout, Mode::DryRun, None, false, &SilentReporter)
            .await
            .unwrap();
        let retry = run(&storage, layout, Mode::DryRun, None, true, &SilentReporter)
            .await
            .unwrap();

        let (Report::DryRun(plain), Report::DryRun(retry)) = (plain, retry) else {
            panic!("a dry run reports a dry run");
        };
        assert_eq!(plain.samples, ["todo"]);
        assert_eq!(plain.candidates, 1);
        assert_eq!(retry.samples, ["skipped", "todo"]);
    }

    /// Applying indexes each candidate, stamps its catalog entry and counts
    /// a document that held nothing to index as still skipped.
    #[tokio::test]
    async fn applying_indexes_and_stamps_each_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        seed(&storage, layout, "one", converted(), true).await;
        seed(&storage, layout, "two", converted(), true).await;
        let server = distill_answering(CHUNKS, false).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let report = run(
            &storage,
            layout,
            Mode::Apply {
                distill: &distill,
                store: ACADEMIC,
            },
            None,
            false,
            &SilentReporter,
        )
        .await
        .unwrap();

        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.candidates, 2);
        assert_eq!(report.indexed, 2);
        assert_eq!(report.still_skipped, 0);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(server.requests().len(), 2);
        let stamp = read_catalog_entry_via(&storage, &layout.catalog_prefix, "one")
            .await
            .unwrap()
            .unwrap()
            .embedding
            .unwrap();
        assert_eq!(stamp.server, STAMP_SERVER);
        assert_eq!(stamp.chunks_indexed, CHUNKS);

        let empty = distill_answering(0, false).await;
        let distill = DistillClient::new(&empty.base).unwrap();
        seed(&storage, layout, "three", converted(), true).await;
        let report = run(
            &storage,
            layout,
            Mode::Apply {
                distill: &distill,
                store: ACADEMIC,
            },
            None,
            false,
            &SilentReporter,
        )
        .await
        .unwrap();
        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.indexed, 0);
        assert_eq!(report.still_skipped, 1);
    }

    /// A document with no markdown, and a server that fails, are listed in
    /// `errors` by stem and do not stop the other documents.
    #[tokio::test]
    async fn a_failing_document_is_reported_and_the_others_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        seed(&storage, layout, "a_no_markdown", converted(), false).await;
        seed(&storage, layout, "b_has_markdown", converted(), true).await;
        let server = distill_answering(CHUNKS, true).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let report = run(
            &storage,
            layout,
            Mode::Apply {
                distill: &distill,
                store: ACADEMIC,
            },
            None,
            false,
            &SilentReporter,
        )
        .await
        .unwrap();

        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.indexed, 0);
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(report.errors[0].starts_with("a_no_markdown: "));
        assert!(report.errors[0].contains("Markdown not found"));
        assert!(report.errors[1].starts_with("b_has_markdown: "));
        assert!(
            !server.requests().is_empty(),
            "the document with markdown was sent"
        );
    }

    /// `--limit` bounds how many documents are indexed, not how many are
    /// counted as candidates.
    #[tokio::test]
    async fn the_limit_bounds_the_documents_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        for stem in ["a", "b", "c"] {
            seed(&storage, layout, stem, converted(), true).await;
        }
        let server = distill_answering(CHUNKS, false).await;
        let distill = DistillClient::new(&server.base).unwrap();

        let report = run(
            &storage,
            layout,
            Mode::Apply {
                distill: &distill,
                store: ACADEMIC,
            },
            Some(2),
            false,
            &SilentReporter,
        )
        .await
        .unwrap();

        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.candidates, 3);
        assert_eq!(report.indexed, 2);
        assert_eq!(server.requests().len(), 2);
    }
}
