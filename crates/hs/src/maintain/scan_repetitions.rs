//! `hs distill scan-repetitions`: find stored markdown that the repetition
//! cleaner would cut heavily.
//!
//! A converter that loops writes the same words again and again. The scan
//! runs the cleaner over each stored document in memory and reports the
//! documents it would cut more than `threshold` times. It changes nothing.

use anyhow::{Context, Result};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use hs_scribe::diag::TruncationCounts;
use serde::Serialize;

/// Documents scanned when `--limit` is not given.
pub(crate) const DEFAULT_LIMIT: usize = 100_000;

/// Cuts a document may take before it is reported, when `--threshold` is not
/// given.
pub(crate) const DEFAULT_THRESHOLD: usize = 20;

/// Characters of context shown around where the cleaned text first differs
/// from the original.
const SNIPPET_WINDOW_CHARS: usize = 120;

/// Documents scanned between two progress lines.
const PROGRESS_EVERY_DOCUMENTS: usize = 500;

/// One document the cleaner would cut heavily.
#[derive(Debug, Serialize)]
pub(crate) struct Flagged {
    pub stem: String,
    pub key: String,
    pub truncations: usize,
    pub truncations_by_pass: TruncationCounts,
    pub offending_snippet: String,
}

/// What a scan found.
#[derive(Debug, Serialize)]
pub(crate) struct ScanReport {
    pub scanned: usize,
    pub threshold: usize,
    pub flagged_count: usize,
    pub flagged: Vec<Flagged>,
}

/// Run the cleaner over the first `limit` markdown documents of the store
/// `layout` describes and report those it cuts more than `threshold` times.
/// A document that cannot be read or is not UTF-8 is an error: a scan that
/// skipped it would report a corpus it had not looked at.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    limit: usize,
    threshold: usize,
    reporter: &dyn Reporter,
) -> Result<ScanReport> {
    let objects = storage
        .list(&layout.markdown_prefix)
        .await
        .with_context(|| format!("listing {}", layout.markdown_prefix))?;

    let mut scanned = 0usize;
    let mut flagged: Vec<Flagged> = Vec::new();
    for object in objects.into_iter().take(limit) {
        if !object.key.ends_with(".md") {
            continue;
        }
        scanned += 1;
        if scanned.is_multiple_of(PROGRESS_EVERY_DOCUMENTS) {
            reporter.status(
                "Scan",
                &format!("{scanned} documents scanned, {} flagged", flagged.len()),
            );
        }
        let bytes = storage
            .get(&object.key)
            .await
            .with_context(|| format!("reading {} failed", object.key))?;
        let original = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", object.key))?;

        // The cleaner is CPU-bound and a large document takes a while.
        let (original, cleaned, breakdown) = tokio::task::spawn_blocking(move || {
            let (cleaned, per_page) = hs_scribe::postprocess::clean_repetitions_per_page(&original);
            let breakdown: TruncationCounts = per_page.iter().copied().sum();
            (original, cleaned, breakdown)
        })
        .await
        .with_context(|| format!("scanning {} failed", object.key))?;

        let truncations = breakdown.total();
        if truncations <= threshold {
            continue;
        }
        let filename = object.key.rsplit('/').next().unwrap_or(&object.key);
        let stem = filename.strip_suffix(".md").unwrap_or(filename).to_string();
        let offending_snippet =
            hs_scribe::postprocess::divergence_snippet(&original, &cleaned, SNIPPET_WINDOW_CHARS)
                .unwrap_or_default();
        flagged.push(Flagged {
            stem,
            key: object.key,
            truncations,
            truncations_by_pass: breakdown,
            offending_snippet,
        });
    }

    Ok(ScanReport {
        scanned,
        threshold,
        flagged_count: flagged.len(),
        flagged,
    })
}

/// `hs distill scan-repetitions`.
pub(crate) async fn cmd(
    limit: Option<usize>,
    threshold: Option<usize>,
    reporter: &dyn Reporter,
) -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (_, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let report = run(
        storage.as_ref(),
        layout,
        limit.unwrap_or(DEFAULT_LIMIT),
        threshold.unwrap_or(DEFAULT_THRESHOLD),
        reporter,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::stores;
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;

    /// Four words written over and over, as a looping converter writes them.
    fn looping_text() -> String {
        "alpha beta gamma delta ".repeat(400)
    }

    const PROSE: &str = "A short paragraph of ordinary prose, with no repeated runs in it.";

    async fn put(storage: &LocalFsStorage, layout: &DocumentLayout, stem: &str, body: &[u8]) {
        storage
            .put(&layout.markdown_key(stem), body.to_vec())
            .await
            .unwrap();
    }

    /// A looping document is reported with its stem, its cut count and the
    /// text where the cleaner first changed it; ordinary prose is not.
    #[tokio::test]
    async fn a_looping_document_is_flagged_and_prose_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        put(&storage, layout, "loop", looping_text().as_bytes()).await;
        put(&storage, layout, "prose", PROSE.as_bytes()).await;

        let report = run(&storage, layout, DEFAULT_LIMIT, 0, &SilentReporter)
            .await
            .unwrap();

        assert_eq!(report.scanned, 2);
        assert_eq!(report.flagged_count, 1);
        let flagged = &report.flagged[0];
        assert_eq!(flagged.stem, "loop");
        assert!(flagged.key.ends_with("loop.md"));
        assert!(flagged.truncations > 0);
        assert_eq!(flagged.truncations, flagged.truncations_by_pass.total());
        assert!(!flagged.offending_snippet.is_empty());
    }

    /// A document cut no more often than the threshold is not reported.
    #[tokio::test]
    async fn a_document_within_the_threshold_is_not_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        put(&storage, layout, "loop", looping_text().as_bytes()).await;

        let report = run(&storage, layout, DEFAULT_LIMIT, usize::MAX, &SilentReporter)
            .await
            .unwrap();

        assert_eq!(report.scanned, 1);
        assert_eq!(report.flagged_count, 0);
    }

    /// `--limit` stops the scan after that many objects, and only markdown
    /// objects are scanned.
    #[tokio::test]
    async fn the_limit_bounds_the_scan_and_only_markdown_is_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        for stem in ["a", "b", "c"] {
            put(&storage, layout, stem, PROSE.as_bytes()).await;
        }
        storage
            .put(
                &format!("{}/notes.txt", layout.markdown_prefix),
                PROSE.as_bytes().to_vec(),
            )
            .await
            .unwrap();

        let all = run(&storage, layout, DEFAULT_LIMIT, 0, &SilentReporter)
            .await
            .unwrap();
        let one = run(&storage, layout, 1, 0, &SilentReporter).await.unwrap();

        assert_eq!(all.scanned, 3);
        assert!(one.scanned <= 1);
    }

    /// A document that is not UTF-8 stops the scan with an error naming it.
    #[tokio::test]
    async fn a_document_that_is_not_utf8_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        put(&storage, layout, "binary", &[0xff, 0xfe, 0x00]).await;

        let err = run(&storage, layout, DEFAULT_LIMIT, 0, &SilentReporter)
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("binary.md"), "{err:#}");
    }
}
