use anyhow::{Context, Result};
use hs_common::reporter::Reporter;
use hs_common::styles::Styles;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::config::Config;
use crate::models::{Paper, ProviderFailure, SearchQuery, SearchResult, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::downloader::PaperDownloader;
use crate::providers::set::ProviderSet;
use crate::services::download::{download_batch, DownloadEvent, OnProgress};

use crate::cli::SortByArg;
use crate::cli::{ProviderArg, SearchTypeArg};
use crate::output;
use hs_common::global_args::GlobalArgs;

#[allow(clippy::too_many_arguments)]
pub async fn run_search(
    query: String,
    date: Option<String>,
    search_type: SearchTypeArg,
    sort_by: SortByArg,
    max_results: u16,
    offset: usize,
    provider: ProviderArg,
    show_abstract: bool,
    min_citations: Option<u64>,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
    styles: &Styles,
    mode: &hs_common::mode::OutputMode,
) -> Result<()> {
    let config = Config::load().context("Failed to load config")?;
    let provider = ProviderSet::new(&config)?.provider(&provider);

    let stage = reporter.begin_stage("Searching", None);
    stage.set_message(&format!("{} for '{}'", provider.name(), query));
    let date_filter = parse_date_arg(date)?;

    if looks_like_doi(&query) {
        return lookup_and_display(&query, "DOI", &*provider, stage, global, reporter, styles)
            .await;
    } else if looks_like_arxiv_id(&query) {
        return lookup_and_display(
            &query, "arXiv ID", &*provider, stage, global, reporter, styles,
        )
        .await;
    };

    let search_type: crate::models::SearchType = search_type.into();

    let search_query = SearchQuery {
        query,
        search_type,
        max_results: max_results as usize,
        offset,
        date_filter,
        sort_by: sort_by.into(),
        min_citations,
    };

    let result = provider
        .search_by_query(&search_query)
        .await
        .context("Search failed")?;

    stage.finish_and_clear();
    warn_provider_failures(&result.provider_failures, reporter);

    if result.papers.is_empty() && !global.is_json() {
        reporter.warn("No papers found. Try broadening your query or removing filters.");
        return Ok(());
    }

    if global.is_json() {
        output::print_json(&result)?;
    } else if matches!(mode, hs_common::mode::OutputMode::Pipe) {
        output::print_search_result_pipe(&result);
    } else {
        output::print_search_result(
            &result,
            styles,
            show_abstract,
            &search_query.query,
            search_query.offset,
        );
    }

    Ok(())
}

pub async fn run_get(
    doi: String,
    provider: ProviderArg,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
    styles: &Styles,
) -> Result<()> {
    let config = Config::load().context("Failed to load config")?;
    let stage = reporter.begin_stage("Looking up", None);
    stage.set_message(&format!("DOI: {}", doi));

    let provider = ProviderSet::new(&config)?.provider(&provider);

    let paper = provider
        .get_by_doi(&doi)
        .await
        .context("DOI lookup failed")?;

    stage.finish_and_clear();

    match paper {
        Some(p) => {
            if global.is_json() {
                output::print_json(&p)?;
            } else {
                output::print_paper(&p, styles);
            }
        }
        None => {
            anyhow::bail!("Not found: {}", doi)
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_download(
    query: Option<String>,
    date: Option<String>,
    doi: Option<String>,
    max_results: u16,
    concurrency: usize,
    search_type: SearchTypeArg,
    provider: ProviderArg,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let config = Config::load().context("Failed to load config")?;

    let providers = ProviderSet::new(&config).context("Failed to create providers")?;

    let storage = config
        .build_storage()
        .context("Failed to build storage backend")?;
    let events = config
        .build_event_bus()
        .await
        .context("Failed to build event bus")?;
    let downloader = PaperDownloader::with_event_bus(
        storage,
        events,
        &config.download,
        providers.download_resolvers(),
    )
    .context("Failed to create downloader")?;
    let downloader: Arc<dyn crate::ports::download_service::DownloadService> = Arc::new(downloader);

    if let Some(doi_str) = doi {
        let stage = reporter.begin_stage("Downloading", None);
        stage.set_message(&format!("DOI: {}", doi_str));

        let result = downloader
            .download_by_doi(&doi_str)
            .await
            .context("Download failed")?;

        stage.finish_and_clear();
        if global.is_json() {
            output::print_json(&result)?;
        } else {
            reporter.finish(&format!(
                "{} ({} bytes)",
                result.file_path.display(),
                result.size_bytes
            ));
        }
    } else if let Some(query_str) = query {
        // For aggregate search, show a counted progress bar; for single provider, a spinner
        let search_total = if matches!(provider, ProviderArg::All) {
            Some(6u64)
        } else {
            None
        };
        let search_stage: Arc<Box<dyn hs_common::reporter::StageHandle>> =
            Arc::new(reporter.begin_counted_stage("Searching", search_total));
        search_stage.set_message(&format!("for '{}'", query_str));

        let search_stage_cb = Arc::clone(&search_stage);
        let on_provider_done = Arc::new(move |name: &str| {
            search_stage_cb.set_message(&format!("{} done", name));
            search_stage_cb.inc(1);
        });

        let provider_impl: Arc<dyn PaperProvider> = if matches!(provider, ProviderArg::All) {
            // Own aggregate (for the progress callback) over the shared members.
            Arc::new(providers.aggregate().on_provider_done(on_provider_done))
        } else {
            providers.provider(&provider)
        };

        let date_filter = parse_date_arg(date)?;

        // Over-request by 50% to compensate for papers without download URLs
        let fetch_count = (max_results as usize).saturating_mul(3) / 2;
        let search_query = SearchQuery {
            query: query_str,
            search_type: search_type.into(),
            max_results: fetch_count,
            offset: 0,
            date_filter,
            sort_by: SortBy::default(),
            min_citations: None,
        };

        let search_result = provider_impl
            .search_by_query(&search_query)
            .await
            .context("Search failed")?;

        search_stage.finish_and_clear();
        warn_provider_failures(&search_result.provider_failures, reporter);

        let total_found = search_result.papers.len();
        if total_found == 0 {
            reporter.warn("No papers found.");
            if !matches!(provider, ProviderArg::All) {
                reporter.warn("Try --provider all to search all sources.");
            }
            return Ok(());
        }

        // Filter out papers with no download path (no URLs and no DOI),
        // then cap to the requested max_results
        let downloadable: Vec<Paper> = search_result
            .papers
            .into_iter()
            .filter(|p| !p.download_urls.is_empty() || p.doi.is_some())
            .take(max_results as usize)
            .collect();

        let skipped_count = total_found - downloadable.len();
        if downloadable.is_empty() {
            reporter.warn(&format!(
                "Found {} papers but none have download URLs or DOIs.",
                total_found
            ));
            return Ok(());
        }

        if skipped_count > 0 {
            reporter.status(
                "Found",
                &format!(
                    "{} downloadable papers ({} without URLs skipped), downloading (concurrency={})...",
                    downloadable.len(), skipped_count, concurrency
                ),
            );
        } else {
            reporter.status(
                "Found",
                &format!(
                    "{} papers, downloading (concurrency={})...",
                    downloadable.len(),
                    concurrency
                ),
            );
        }

        // Replace search_result.papers with filtered list
        let search_result = SearchResult {
            papers: downloadable,
            total_results: search_result.total_results,
            next_offset: search_result.next_offset,
            provider: search_result.provider,
            provider_failures: search_result.provider_failures,
        };

        // Overall progress counter (ephemeral — cleared when done)
        let total_papers = search_result.papers.len();
        let overall: Arc<Box<dyn hs_common::reporter::StageHandle>> =
            Arc::new(reporter.begin_counted_stage("Downloading", Some(total_papers as u64)));

        // Per-download progress bars (ephemeral — cleared on completion)
        let bars: Arc<Mutex<HashMap<usize, Box<dyn hs_common::reporter::StageHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let bars_ref = Arc::clone(&bars);
        let reporter_ref = Arc::clone(reporter);
        let overall_ref = Arc::clone(&overall);

        let progress: Option<OnProgress> =
            Some(Arc::new(move |event: DownloadEvent| match event {
                DownloadEvent::Started { index, title, .. } => {
                    if let Ok(mut bars) = bars_ref.lock() {
                        let stage = reporter_ref.begin_stage(&title, None);
                        bars.insert(index, stage);
                    }
                }
                DownloadEvent::Progress {
                    index,
                    bytes_downloaded,
                    bytes_total,
                    ..
                } => {
                    if let Ok(bars) = bars_ref.lock() {
                        if let Some(bar) = bars.get(&index) {
                            if let Some(total) = bytes_total {
                                bar.set_length(total);
                            }
                            bar.set_position(bytes_downloaded);
                        }
                    }
                }
                DownloadEvent::Completed { index, .. } => {
                    if let Ok(mut bars) = bars_ref.lock() {
                        if let Some(bar) = bars.remove(&index) {
                            bar.finish_and_clear();
                        }
                    }
                    overall_ref.inc(1);
                }
                DownloadEvent::Failed { index, .. } => {
                    if let Ok(mut bars) = bars_ref.lock() {
                        if let Some(bar) = bars.remove(&index) {
                            bar.finish_and_clear();
                        }
                    }
                    overall_ref.inc(1);
                }
                DownloadEvent::Skipped { index, .. } => {
                    if let Ok(mut bars) = bars_ref.lock() {
                        if let Some(bar) = bars.remove(&index) {
                            bar.finish_and_clear();
                        }
                    }
                    overall_ref.inc(1);
                }
            }));

        let batch_result =
            download_batch(downloader, search_result.papers, concurrency, progress).await;
        let batch_result = match batch_result {
            Ok(r) => r,
            Err(e) => {
                overall.finish_and_clear();
                return Err(e).context("Download failed");
            }
        };

        overall.finish_and_clear();

        if global.is_json() {
            output::print_json(&batch_result)?;
        } else {
            let total_bytes: u64 = batch_result
                .succeeded
                .iter()
                .chain(batch_result.skipped.iter())
                .map(|r| r.size_bytes)
                .sum();
            reporter.finish(&format!(
                "\n{}/{} downloaded, {} already exist, {} unavailable ({})",
                batch_result.succeeded.len(),
                batch_result.total_requested,
                batch_result.skipped.len(),
                batch_result.failed.len(),
                format_bytes(total_bytes),
            ));
        }

        if !batch_result.failed.is_empty() {
            anyhow::bail!(
                "{} of {} downloads failed",
                batch_result.failed.len(),
                batch_result.total_requested
            );
        }
    } else {
        anyhow::bail!("Provide either a search query or --doi");
    }

    Ok(())
}

/// Print one warning line per provider that failed while others answered.
/// (A search where *every* provider failed is an `Err`, not a warning.)
fn warn_provider_failures(failures: &[ProviderFailure], reporter: &Arc<dyn Reporter>) {
    for f in failures {
        reporter.warn(&format!("{} failed: {}", f.provider, f.error));
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

async fn lookup_and_display(
    query: &str,
    label: &str,
    provider: &dyn PaperProvider,
    stage: Box<dyn hs_common::reporter::StageHandle>,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
    styles: &Styles,
) -> Result<()> {
    let paper = provider
        .get_by_doi(query)
        .await
        .context(format!("{} lookup failed", label))?;

    stage.finish_and_clear();

    match paper {
        Some(p) => {
            if global.is_json() {
                output::print_json(&p)?;
            } else {
                output::print_paper(&p, styles);
            }
        }
        None => {
            reporter.warn(&format!("No paper found for {}: {}", label, query));
        }
    }
    Ok(())
}

/// DOI format: starts with "10." followed by a registrant code and a slash
/// e.g., "10.1038/s41576-024-00001-2"
fn looks_like_doi(query: &str) -> bool {
    query.starts_with("10.") && query.contains('/')
}

fn parse_date_arg(date: Option<String>) -> Result<Option<crate::models::DateFilter>> {
    date.map(|d| crate::models::DateFilter::parse(&d))
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid date filter: {}", e))
}

/// arXiv ID format: digits, a dot, then more digits
/// e.g., "2408.13479" or "2408.13479v5"
fn looks_like_arxiv_id(query: &str) -> bool {
    let stripped = query.split('v').next().unwrap_or(query);
    let parts: Vec<&str> = stripped.splitn(2, '.').collect();
    parts.len() == 2
        && parts[0].len() == 4
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
}
