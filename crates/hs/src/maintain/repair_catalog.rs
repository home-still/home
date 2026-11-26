//! `hs pipeline repair-catalog`: where the catalog and the storage disagree.
//!
//! The command reports; it changes nothing. It reads the sources, the
//! markdown and every catalog entry once, then looks for seven kinds of
//! disagreement in that one snapshot.

use anyhow::{Context, Result};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use serde::Serialize;

/// Stems shown as examples for each kind of disagreement.
const SAMPLE_LIMIT: usize = 10;

/// Catalog entries read between two progress lines.
const CATALOG_PROGRESS_EVERY: usize = 500;

/// How many kinds of disagreement the command looks for.
const SCAN_COUNT: usize = 7;

/// Source files with no catalog entry.
#[derive(Debug, Serialize)]
pub(crate) struct DiskNoCatalog {
    pub orphans_found: usize,
    pub would_repair: usize,
    pub samples: Vec<String>,
}

/// Catalog entries that say the document was converted, with no markdown.
#[derive(Debug, Serialize)]
pub(crate) struct CatalogNoMarkdown {
    pub orphans_found: usize,
    pub would_clear_conversion: usize,
    pub samples: Vec<String>,
}

/// Catalog entries with neither a source nor markdown.
#[derive(Debug, Serialize)]
pub(crate) struct CatalogNoSource {
    pub orphans_found: usize,
    pub would_delete: usize,
    pub samples: Vec<String>,
}

/// Entries where a stage flag is missing but storage shows the stage done.
#[derive(Debug, Serialize)]
pub(crate) struct FlagDrift {
    pub drift_found: usize,
    pub would_backfill_conversion: usize,
    pub would_backfill_downloaded_at: usize,
    pub samples: Vec<String>,
}

/// Entries whose timestamps still carry the stamp of one earlier batch.
#[derive(Debug, Serialize)]
pub(crate) struct FlagDriftResync {
    pub candidates_found: usize,
    pub would_resync_downloaded_at: usize,
    pub would_resync_conversion: usize,
    pub samples: Vec<String>,
}

/// One entry whose recorded markdown path is not where the markdown is.
#[derive(Debug, Serialize)]
pub(crate) struct MdPathSample {
    pub stem: String,
    pub stale_path: String,
    pub resolved_path: String,
}

/// Entries whose `markdown_path` disagrees with the markdown's real key.
#[derive(Debug, Serialize)]
pub(crate) struct MdPathDrift {
    pub drift_found: usize,
    pub would_rewrite: usize,
    pub samples: Vec<MdPathSample>,
}

/// One document that has a source and was never converted.
#[derive(Debug, Serialize)]
pub(crate) struct StuckSample {
    pub stem: String,
    pub source_ext: String,
}

/// Documents with a source, no conversion stamp and no failure stamp.
#[derive(Debug, Serialize)]
pub(crate) struct StuckConvert {
    pub stuck_found: usize,
    pub would_emit: usize,
    pub pdf_candidates: usize,
    pub html_candidates: usize,
    pub samples: Vec<StuckSample>,
}

/// What a run found, one entry for each kind of disagreement.
#[derive(Debug, Serialize)]
pub(crate) struct RepairReport {
    pub disk_no_catalog: DiskNoCatalog,
    pub catalog_no_markdown: CatalogNoMarkdown,
    pub catalog_no_source: CatalogNoSource,
    pub flag_drift: FlagDrift,
    pub flag_drift_resync: FlagDriftResync,
    pub md_path_drift: MdPathDrift,
    pub stuck_convert: StuckConvert,
}

/// The first [`SAMPLE_LIMIT`] stems of a list, as examples.
fn first_stems<'a>(stems: impl Iterator<Item = &'a String>) -> Vec<String> {
    stems.take(SAMPLE_LIMIT).cloned().collect()
}

/// Look for the seven kinds of disagreement between the catalog and the
/// storage of the store `layout` describes. `limit` caps how many entries
/// each `would_*` count reports, as a repair run limited to that many would.
/// Nothing is written.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    limit: Option<usize>,
    reporter: &dyn Reporter,
) -> Result<RepairReport> {
    let limit = limit.unwrap_or(usize::MAX);
    let snapshot = hs_common::status::build_repair_snapshot(
        storage,
        &layout.source_prefix,
        &layout.catalog_prefix,
        &layout.markdown_prefix,
        hs_common::catalog::CATALOG_FETCH_CONCURRENCY,
        |done, total| {
            if done.is_multiple_of(CATALOG_PROGRESS_EVERY) || done == total {
                reporter.status("Snapshot", &format!("{done}/{total} catalog entries read"));
            }
        },
    )
    .await
    .context("reading the sources, the markdown and the catalog")?;

    let scan = |number: usize, name: &str| {
        reporter.status("Scan", &format!("{number}/{SCAN_COUNT}: {name}"));
    };

    scan(1, "source files with no catalog entry");
    let disk_orphans =
        hs_common::status::list_orphan_document_stems(&snapshot.papers, &snapshot.catalog);

    scan(2, "catalog entries without markdown");
    let md_orphans = hs_common::status::list_catalog_rows_without_markdown(
        &snapshot.catalog,
        &snapshot.markdown,
    );

    scan(3, "catalog entries without a source");
    let phantom_orphans = hs_common::status::list_catalog_rows_without_source(
        &snapshot.papers,
        &snapshot.catalog,
        &snapshot.markdown,
    );

    scan(4, "flag drift");
    let drift_rows = hs_common::status::list_catalog_flag_drift(
        &snapshot.papers,
        &snapshot.catalog,
        &snapshot.markdown,
    );

    scan(5, "flag drift resync");
    let resync_rows = hs_common::status::list_catalog_flag_drift_resync_candidates(
        &snapshot.papers,
        &snapshot.catalog,
        &snapshot.markdown,
    );

    scan(6, "markdown path drift");
    let md_path_drift_rows = hs_common::status::list_catalog_rows_with_md_path_drift(
        &snapshot.catalog,
        &snapshot.markdown,
    );

    scan(7, "documents never converted");
    let stuck_rows =
        hs_common::status::list_catalog_stuck_convert(&snapshot.papers, &snapshot.catalog);

    let drift_conversion_total = drift_rows
        .iter()
        .filter(|r| r.conversion_missing_with_markdown)
        .count();
    let drift_download_total = drift_rows
        .iter()
        .filter(|r| r.download_stamp_missing_with_source)
        .count();
    let resync_download_total = resync_rows
        .iter()
        .filter(|r| r.resync_download.is_some())
        .count();
    let resync_conversion_total = resync_rows
        .iter()
        .filter(|r| r.resync_conversion.is_some())
        .count();

    Ok(RepairReport {
        disk_no_catalog: DiskNoCatalog {
            orphans_found: disk_orphans.len(),
            would_repair: disk_orphans.len().min(limit),
            samples: disk_orphans
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|(stem, _)| stem.clone())
                .collect(),
        },
        catalog_no_markdown: CatalogNoMarkdown {
            orphans_found: md_orphans.len(),
            would_clear_conversion: md_orphans.len().min(limit),
            samples: first_stems(md_orphans.iter()),
        },
        catalog_no_source: CatalogNoSource {
            orphans_found: phantom_orphans.len(),
            would_delete: phantom_orphans.len().min(limit),
            samples: first_stems(phantom_orphans.iter()),
        },
        flag_drift: FlagDrift {
            drift_found: drift_rows.len(),
            would_backfill_conversion: drift_conversion_total.min(limit),
            would_backfill_downloaded_at: drift_download_total.min(limit),
            samples: first_stems(drift_rows.iter().map(|r| &r.stem)),
        },
        flag_drift_resync: FlagDriftResync {
            candidates_found: resync_rows.len(),
            would_resync_downloaded_at: resync_download_total.min(limit),
            would_resync_conversion: resync_conversion_total.min(limit),
            samples: first_stems(resync_rows.iter().map(|r| &r.stem)),
        },
        md_path_drift: MdPathDrift {
            drift_found: md_path_drift_rows.len(),
            would_rewrite: md_path_drift_rows.len().min(limit),
            samples: md_path_drift_rows
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|r| MdPathSample {
                    stem: r.stem.clone(),
                    stale_path: r.stale_path.clone(),
                    resolved_path: r.resolved_path.clone(),
                })
                .collect(),
        },
        stuck_convert: StuckConvert {
            stuck_found: stuck_rows.len(),
            would_emit: stuck_rows.len().min(limit),
            pdf_candidates: stuck_rows.iter().filter(|r| r.source_ext == "pdf").count(),
            html_candidates: stuck_rows.iter().filter(|r| r.source_ext == "html").count(),
            samples: stuck_rows
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|r| StuckSample {
                    stem: r.stem.clone(),
                    source_ext: r.source_ext.clone(),
                })
                .collect(),
        },
    })
}

/// `hs pipeline repair-catalog`.
pub(crate) async fn cmd(limit: Option<usize>, reporter: &dyn Reporter) -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (_, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let report = run(storage.as_ref(), layout, limit, reporter).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::{conversion, stores};
    use hs_common::catalog::{write_catalog_entry_via, CatalogEntry};
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;

    async fn seed_row(
        storage: &LocalFsStorage,
        layout: &DocumentLayout,
        stem: &str,
        entry: CatalogEntry,
    ) {
        write_catalog_entry_via(storage, &layout.catalog_prefix, stem, &entry)
            .await
            .unwrap();
    }

    /// A source with no catalog entry, a converted entry whose markdown is
    /// gone, an entry with nothing behind it and a source that was never
    /// converted are each reported under their own direction, and a healthy
    /// document is reported under none.
    #[tokio::test]
    async fn each_disagreement_is_reported_under_its_own_direction() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();

        storage
            .put(&layout.source_key("loose", "pdf"), b"%PDF-1.4".to_vec())
            .await
            .unwrap();

        seed_row(
            &storage,
            layout,
            "lost",
            CatalogEntry {
                conversion: Some(conversion()),
                ..Default::default()
            },
        )
        .await;
        storage
            .put(&layout.source_key("lost", "pdf"), b"%PDF-1.4".to_vec())
            .await
            .unwrap();

        seed_row(&storage, layout, "phantom", CatalogEntry::default()).await;

        seed_row(&storage, layout, "waiting", CatalogEntry::default()).await;
        storage
            .put(&layout.source_key("waiting", "html"), b"<html>".to_vec())
            .await
            .unwrap();

        seed_row(
            &storage,
            layout,
            "healthy",
            CatalogEntry {
                conversion: Some(conversion()),
                ..Default::default()
            },
        )
        .await;
        storage
            .put(&layout.source_key("healthy", "pdf"), b"%PDF-1.4".to_vec())
            .await
            .unwrap();
        storage
            .put(&layout.markdown_key("healthy"), b"# Healthy".to_vec())
            .await
            .unwrap();

        let report = run(&storage, layout, None, &SilentReporter).await.unwrap();

        assert_eq!(report.disk_no_catalog.samples, ["loose"]);
        assert_eq!(report.catalog_no_markdown.samples, ["lost"]);
        assert_eq!(report.catalog_no_source.samples, ["phantom"]);
        assert_eq!(report.stuck_convert.stuck_found, 1);
        assert_eq!(report.stuck_convert.html_candidates, 1);
        assert_eq!(report.stuck_convert.pdf_candidates, 0);
        assert_eq!(report.stuck_convert.samples[0].stem, "waiting");
    }

    /// `--limit` caps what a repair run would touch, not what the command
    /// finds.
    #[tokio::test]
    async fn the_limit_caps_the_would_counts_and_not_the_found_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        for stem in ["a1", "a2", "a3"] {
            seed_row(&storage, layout, stem, CatalogEntry::default()).await;
        }

        let report = run(&storage, layout, Some(2), &SilentReporter)
            .await
            .unwrap();

        assert_eq!(report.catalog_no_source.orphans_found, 3);
        assert_eq!(report.catalog_no_source.would_delete, 2);
    }

    /// An empty store has nothing to report, and that is not an error.
    #[tokio::test]
    async fn an_empty_store_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();

        let report = run(&storage, layout, None, &SilentReporter).await.unwrap();

        assert_eq!(report.disk_no_catalog.orphans_found, 0);
        assert_eq!(report.catalog_no_markdown.orphans_found, 0);
        assert_eq!(report.catalog_no_source.orphans_found, 0);
        assert_eq!(report.flag_drift.drift_found, 0);
        assert_eq!(report.md_path_drift.drift_found, 0);
        assert_eq!(report.stuck_convert.stuck_found, 0);
    }
}
