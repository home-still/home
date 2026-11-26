//! `hs pipeline backfill-titles`: give catalog entries that have a DOI and no
//! title the metadata the paper providers hold.
//!
//! Entries made by the inbox watcher, by a conversion run from an event or by
//! a repair have no title until a metadata lookup happens. With `--dry-run`
//! the command only counts what it would look up.

use anyhow::{Context, Result};
use hs_common::catalog::{AuthorEntry, CatalogEntry, RepairMeta};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use paper::ports::provider::PaperProvider;
use serde::Serialize;

/// Stems shown as examples in the report.
const SAMPLE_LIMIT: usize = 10;

/// Provider lookups between two progress lines.
const PROGRESS_EVERY_LOOKUPS: usize = 25;

/// The `reason` recorded on the repair stamp of an entry that got its title.
const REPAIR_REASON: &str = "title backfilled via paper aggregate";

/// What a run does with the entries it selects.
pub(crate) enum Mode<'a> {
    /// Count the entries a run would look up; contact no provider.
    DryRun,
    /// Look each entry up with `provider` and write what it returns.
    Apply { provider: &'a dyn PaperProvider },
}

/// The report of a run with `--dry-run`.
#[derive(Debug, Serialize)]
pub(crate) struct DryRunReport {
    pub dry_run: bool,
    pub candidates: usize,
    pub would_backfill: usize,
    pub samples: Vec<String>,
}

/// The report of a run without `--dry-run`.
#[derive(Debug, Serialize)]
pub(crate) struct AppliedReport {
    pub dry_run: bool,
    pub candidates: usize,
    pub backfilled: u64,
    pub no_metadata: u64,
    pub samples: Vec<String>,
    pub errors: Vec<String>,
}

/// What a run found or did.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Report {
    DryRun(DryRunReport),
    Applied(AppliedReport),
}

/// Whether an entry has a DOI and no title (a title of only spaces is none).
fn needs_title(entry: &CatalogEntry) -> bool {
    let has_doi = entry.doi.as_ref().is_some_and(|d| !d.trim().is_empty());
    has_doi && !has_title(entry)
}

fn has_title(entry: &CatalogEntry) -> bool {
    entry.title.as_ref().is_some_and(|t| !t.trim().is_empty())
}

/// What one lookup came to.
enum Outcome {
    Backfilled,
    NoMetadata,
    /// The entry got a title while the run was looking others up.
    AlreadyTitled,
}

/// Look `stem` up by `doi` and write the title and the other metadata the
/// entry lacks onto the entry as it is now.
async fn backfill_one(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    provider: &dyn PaperProvider,
    stem: &str,
    doi: &str,
    now: &str,
) -> Result<Outcome> {
    let meta = match provider
        .get_by_doi(doi)
        .await
        .map_err(|e| anyhow::anyhow!("provider error: {e}"))?
    {
        Some(meta) if !meta.title.trim().is_empty() => meta,
        _ => return Ok(Outcome::NoMetadata),
    };

    // The listing is minutes old by now (one provider round trip per entry):
    // write onto the entry as it is, not as it was, or a conversion or
    // embedding stamp that landed in between is erased.
    let mut entry =
        hs_common::catalog::read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
            .await
            .context("catalog re-read failed")?
            .context("catalog entry vanished before the write")?;
    if has_title(&entry) {
        return Ok(Outcome::AlreadyTitled);
    }

    entry.title = Some(meta.title.clone());
    if entry.authors.is_empty() && !meta.authors.is_empty() {
        entry.authors = meta
            .authors
            .iter()
            .map(|a| AuthorEntry {
                name: a.name.clone(),
            })
            .collect();
    }
    if entry.publication_date.is_none() {
        entry.publication_date = meta.publication_date.map(|d| d.to_string());
    }
    if entry.abstract_text.is_none() {
        entry.abstract_text = meta.abstract_text.clone();
    }
    if entry.cited_by_count.is_none() {
        entry.cited_by_count = meta.cited_by_count;
    }
    entry.repair = Some(RepairMeta {
        repaired_at: now.to_string(),
        reason: REPAIR_REASON.to_string(),
    });
    hs_common::catalog::write_catalog_entry_via(storage, &layout.catalog_prefix, stem, &entry)
        .await
        .context("write failed")?;
    Ok(Outcome::Backfilled)
}

/// Find the catalog entries of the store `layout` describes that have a DOI
/// and no title, and, in [`Mode::Apply`], look up and write the metadata of
/// the first `limit` of them. One entry's failure does not stop the others:
/// it is reported and listed in `errors`.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    mode: Mode<'_>,
    limit: Option<usize>,
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

    let mut candidates: Vec<(String, CatalogEntry)> = entries
        .into_iter()
        .filter(|(_, _, entry)| needs_title(entry))
        .map(|(stem, _, entry)| (stem, entry))
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    let total = candidates.len();
    let selected: Vec<(String, CatalogEntry)> = candidates
        .into_iter()
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    let samples: Vec<String> = selected
        .iter()
        .take(SAMPLE_LIMIT)
        .map(|(stem, _)| stem.clone())
        .collect();

    let provider = match mode {
        Mode::DryRun => {
            return Ok(Report::DryRun(DryRunReport {
                dry_run: true,
                candidates: total,
                would_backfill: selected.len(),
                samples,
            }));
        }
        Mode::Apply { provider } => provider,
    };

    let now = chrono::Utc::now().to_rfc3339();
    let selected_count = selected.len();
    let mut backfilled = 0u64;
    let mut no_metadata = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for (done, (stem, listed)) in selected.into_iter().enumerate() {
        let Some(doi) = listed.doi.as_deref() else {
            continue;
        };
        match backfill_one(storage, layout, provider, &stem, doi, &now).await {
            Ok(Outcome::Backfilled) => backfilled += 1,
            Ok(Outcome::NoMetadata) => no_metadata += 1,
            Ok(Outcome::AlreadyTitled) => {}
            Err(e) => {
                let message = format!("{stem}: {e:#}");
                reporter.warn(&message);
                errors.push(message);
            }
        }
        let looked_up = done + 1;
        if looked_up.is_multiple_of(PROGRESS_EVERY_LOOKUPS) || looked_up == selected_count {
            reporter.status(
                "Progress",
                &format!("{looked_up}/{selected_count}: backfilled {backfilled}, no metadata {no_metadata}, errors {}", errors.len()),
            );
        }
    }

    Ok(Report::Applied(AppliedReport {
        dry_run: false,
        candidates: total,
        backfilled,
        no_metadata,
        samples,
        errors,
    }))
}

/// `hs pipeline backfill-titles`.
pub(crate) async fn cmd(
    dry_run: bool,
    limit: Option<usize>,
    reporter: &dyn Reporter,
) -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (_, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let report = if !dry_run {
        let config = paper::config::Config::load().context("reading the paper config")?;
        let providers = paper::providers::set::ProviderSet::new(&config)
            .context("building the paper providers")?;
        let provider = providers.provider(&paper::cli::ProviderArg::All);
        run(
            storage.as_ref(),
            layout,
            Mode::Apply {
                provider: provider.as_ref(),
            },
            limit,
            reporter,
        )
        .await?
    } else {
        run(storage.as_ref(), layout, Mode::DryRun, limit, reporter).await?
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    if let Report::Applied(applied) = &report {
        if !applied.errors.is_empty() {
            anyhow::bail!(
                "{} catalog entries could not be backfilled; {} were",
                applied.errors.len(),
                applied.backfilled
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::{conversion, stores};
    use async_trait::async_trait;
    use hs_common::catalog::write_catalog_entry_via;
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;
    use paper::error::PaperError;
    use paper::models::{Author, Paper, SearchQuery, SearchResult, SearchType};

    /// What the stand-in provider answers for one DOI.
    #[derive(Clone)]
    enum Answer {
        Found(Paper),
        Unknown,
        Fails,
    }

    /// A provider that answers from a fixed table, and not at all for a DOI
    /// that is not in it.
    struct FakeProvider(Vec<(String, Answer)>);

    #[async_trait]
    impl PaperProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn supported_search_types(&self) -> Vec<SearchType> {
            vec![SearchType::DOI]
        }

        async fn search_by_query(&self, _query: &SearchQuery) -> Result<SearchResult, PaperError> {
            Err(PaperError::ProviderUnavailable(
                "the stand-in only looks up DOIs".into(),
            ))
        }

        async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
            match self.0.iter().find(|(known, _)| known == doi) {
                Some((_, Answer::Found(paper))) => Ok(Some(paper.clone())),
                Some((_, Answer::Fails)) => Err(PaperError::ProviderUnavailable(
                    "the provider is down".into(),
                )),
                Some((_, Answer::Unknown)) | None => Ok(None),
            }
        }
    }

    fn paper(title: &str) -> Paper {
        Paper {
            id: "id".into(),
            title: title.into(),
            authors: vec![Author {
                name: "Ada Lovelace".into(),
                affiliations: Vec::new(),
            }],
            abstract_text: Some("An abstract.".into()),
            publication_date: None,
            doi: None,
            download_urls: Vec::new(),
            cited_by_count: Some(7),
            source: "fake".into(),
        }
    }

    async fn seed(
        storage: &LocalFsStorage,
        layout: &DocumentLayout,
        stem: &str,
        entry: CatalogEntry,
    ) {
        write_catalog_entry_via(storage, &layout.catalog_prefix, stem, &entry)
            .await
            .unwrap();
    }

    async fn row(storage: &LocalFsStorage, layout: &DocumentLayout, stem: &str) -> CatalogEntry {
        hs_common::catalog::read_catalog_entry_via(storage, &layout.catalog_prefix, stem)
            .await
            .unwrap()
            .unwrap()
    }

    fn with_doi(doi: &str) -> CatalogEntry {
        CatalogEntry {
            doi: Some(doi.into()),
            ..Default::default()
        }
    }

    /// With dry-run mode nothing is looked up and nothing is written; only
    /// the entries with a DOI and no usable title are counted.
    #[tokio::test]
    async fn a_dry_run_counts_the_entries_that_need_a_title_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        seed(&storage, layout, "no_title", with_doi("10.1/a")).await;
        seed(
            &storage,
            layout,
            "blank_title",
            CatalogEntry {
                title: Some("   ".into()),
                ..with_doi("10.1/b")
            },
        )
        .await;
        seed(
            &storage,
            layout,
            "titled",
            CatalogEntry {
                title: Some("Already".into()),
                ..with_doi("10.1/c")
            },
        )
        .await;
        seed(&storage, layout, "no_doi", CatalogEntry::default()).await;

        let report = run(&storage, layout, Mode::DryRun, None, &SilentReporter)
            .await
            .unwrap();

        let Report::DryRun(report) = report else {
            panic!("a dry run reports a dry run");
        };
        assert_eq!(report.candidates, 2);
        assert_eq!(report.would_backfill, 2);
        assert_eq!(report.samples, ["blank_title", "no_title"]);
        assert_eq!(row(&storage, layout, "no_title").await.title, None);
    }

    /// An entry gets the title and the missing metadata, keeps what it
    /// already had, and is stamped as repaired; an unknown DOI is counted
    /// and a failing provider is reported, without stopping the others.
    #[tokio::test]
    async fn applying_writes_the_metadata_and_reports_each_kind_of_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        seed(
            &storage,
            layout,
            "found",
            CatalogEntry {
                conversion: Some(conversion()),
                abstract_text: Some("My own abstract.".into()),
                ..with_doi("10.1/found")
            },
        )
        .await;
        seed(&storage, layout, "unknown", with_doi("10.1/unknown")).await;
        seed(&storage, layout, "failing", with_doi("10.1/failing")).await;
        let provider = FakeProvider(vec![
            ("10.1/found".into(), Answer::Found(paper("The Title"))),
            ("10.1/unknown".into(), Answer::Unknown),
            ("10.1/failing".into(), Answer::Fails),
        ]);

        let report = run(
            &storage,
            layout,
            Mode::Apply {
                provider: &provider,
            },
            None,
            &SilentReporter,
        )
        .await
        .unwrap();

        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.candidates, 3);
        assert_eq!(report.backfilled, 1);
        assert_eq!(report.no_metadata, 1);
        assert_eq!(report.errors.len(), 1);
        assert!(
            report.errors[0].starts_with("failing: "),
            "{:?}",
            report.errors
        );

        let found = row(&storage, layout, "found").await;
        assert_eq!(found.title.as_deref(), Some("The Title"));
        assert_eq!(found.authors.len(), 1);
        assert_eq!(found.authors[0].name, "Ada Lovelace");
        assert_eq!(found.cited_by_count, Some(7));
        assert_eq!(
            found.abstract_text.as_deref(),
            Some("My own abstract."),
            "a field the entry already has is kept"
        );
        assert!(found.conversion.is_some(), "the conversion stamp is kept");
        assert_eq!(found.repair.unwrap().reason, REPAIR_REASON);
        assert_eq!(row(&storage, layout, "unknown").await.title, None);
    }

    /// `--limit` bounds how many providers lookups a run makes.
    #[tokio::test]
    async fn the_limit_bounds_the_lookups() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        for stem in ["a", "b", "c"] {
            seed(&storage, layout, stem, with_doi(&format!("10.1/{stem}"))).await;
        }
        let provider = FakeProvider(
            ["a", "b", "c"]
                .iter()
                .map(|s| (format!("10.1/{s}"), Answer::Found(paper("T"))))
                .collect(),
        );

        let report = run(
            &storage,
            layout,
            Mode::Apply {
                provider: &provider,
            },
            Some(2),
            &SilentReporter,
        )
        .await
        .unwrap();

        let Report::Applied(report) = report else {
            panic!("an applied run reports what it did");
        };
        assert_eq!(report.candidates, 3);
        assert_eq!(report.backfilled, 2);
    }
}
