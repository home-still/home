use anyhow::{Context, Result};
use hs_style::reporter::Reporter;
use hs_style::styles::Styles;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::Config;
use crate::models::SearchQuery;
use crate::models::SortBy;
use crate::ports::provider::PaperProvider;
use crate::providers::arxiv::ArxivProvider;
use crate::providers::downloader::PaperDownloader;
use crate::providers::openalex::OpenAlexProvider;
use crate::providers::resilient::ResilientProvider;
use crate::resilience::circuit_breaker::new_circuit_breaker;
use crate::services::download::{download_batch, DownloadEvent, OnProgress};
use crate::services::search::AggregateProvider;

use crate::cli::SortByArg;
use crate::cli::{ProviderArg, SearchTypeArg};
use crate::output;
use hs_style::global_args::GlobalArgs;

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
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
    styles: &Styles,
) -> Result<()> {
    let config = Config::load().context("Failed to load config")?;
    let provider = make_provider(&provider, &config)?;

    let stage = reporter.begin_stage("Searching", None);
    stage.set_message(&format!("{} for:{}", provider.name(), query));

    let date_filter = date
        .map(|d| crate::models::DateFilter::parse(&d))
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid date filter: {}", e))?;

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
    };

    let result = provider
        .search(&search_query)
        .await
        .context("Search failed")?;

    stage.finish_and_clear();

    if global.is_json() {
        output::print_json(&result)?;
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

    let provider = make_provider(&provider, &config)?;

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

    let downloader = PaperDownloader::new(config.download_path.clone(), &config.download)
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
        let provider_impl = make_provider(&provider, &config)?;

        let search_stage = reporter.begin_stage("Searching", None);
        search_stage.set_message(&format!("{} for {}", provider_impl.name(), query_str));

        let date_filter = date
            .map(|d| crate::models::DateFilter::parse(&d))
            .transpose()
            .map_err(|e| anyhow::anyhow!("Invalid date filter: {}", e))?;

        let search_query = SearchQuery {
            query: query_str,
            search_type: search_type.into(),
            max_results: max_results as usize,
            offset: 0,
            date_filter,
            sort_by: SortBy::default(),
        };

        let search_result = provider_impl
            .search(&search_query)
            .await
            .context("Search failed")?;

        search_stage.finish_and_clear();

        let paper_count = search_result.papers.len();
        if paper_count == 0 {
            reporter.warn("No papers found.");
            return Ok(());
        }

        reporter.status(
            "Found",
            &format!(
                "{} papers, downloading (concurrency={})...",
                paper_count, concurrency
            ),
        );

        let bars: Arc<Mutex<HashMap<usize, Box<dyn hs_style::reporter::StageHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let bars_ref = Arc::clone(&bars);
        let reporter_ref = Arc::clone(reporter);

        let progress: Option<OnProgress> =
            Some(Arc::new(move |event: DownloadEvent| match event {
                DownloadEvent::Started { index, title, .. } => {
                    let stage = reporter_ref.begin_stage(&title, None);
                    bars_ref.lock().unwrap().insert(index, stage);
                }
                DownloadEvent::Progress {
                    index,
                    bytes_downloaded,
                    bytes_total,
                    ..
                } => {
                    if let Some(bar) = bars_ref.lock().unwrap().get(&index) {
                        if let Some(total) = bytes_total {
                            bar.set_length(total);
                        }
                        bar.set_position(bytes_downloaded);
                    }
                }
                DownloadEvent::Completed {
                    index, size_bytes, ..
                } => {
                    if let Some(bar) = bars_ref.lock().unwrap().remove(&index) {
                        bar.finish_with_message(&format_bytes(size_bytes));
                    }
                }
                DownloadEvent::Failed { index, error, .. } => {
                    if let Some(bar) = bars_ref.lock().unwrap().remove(&index) {
                        bar.finish_with_message(&format!("FAILED: {}", error));
                    }
                }
            }));

        let batch_result =
            download_batch(downloader, search_result.papers, concurrency, progress).await;

        if global.is_json() {
            output::print_json(&batch_result)?;
        } else {
            reporter.finish(&format!(
                "\nCompleted: {}/{} succeeded, {} failed",
                batch_result.succeeded.len(),
                batch_result.total_requested,
                batch_result.failed.len(),
            ));
            if !batch_result.failed.is_empty() {
                for f in &batch_result.failed {
                    reporter.warn(&format!("{} -- {}", f.paper_id, f.error));
                }
            }
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

fn make_provider(provider: &ProviderArg, config: &Config) -> Result<Box<dyn PaperProvider>> {
    match provider {
        ProviderArg::Arxiv => {
            let inner = ArxivProvider::new(&config.providers.arxiv)
                .context("Failed to create arXiv provider")?;
            let interval = Duration::from_millis(config.providers.arxiv.rate_limit_interval_ms);
            let cb = new_circuit_breaker(&config.resilience);
            let resilient =
                ResilientProvider::new(Box::new(inner), interval, cb, config.resilience.clone());

            Ok(Box::new(resilient))
        }
        ProviderArg::OpenAlex => {
            let inner = OpenAlexProvider::new(&config.providers.openalex)
                .context("Failed to create OpenAlex provider")?;
            let interval = Duration::from_millis(config.providers.openalex.rate_limit_interval_ms);
            let cb = new_circuit_breaker(&config.resilience);
            let resilient =
                ResilientProvider::new(Box::new(inner), interval, cb, config.resilience.clone());
            Ok(Box::new(resilient))
        }
        ProviderArg::All => {
            // Arxiv
            let arxiv_inner = ArxivProvider::new(&config.providers.arxiv)
                .context("Failed to create arXiv provider")?;
            let arxiv_interval =
                Duration::from_millis(config.providers.arxiv.rate_limit_interval_ms);
            let arxiv_cb = new_circuit_breaker(&config.resilience);
            let arxiv: Box<dyn PaperProvider> = Box::new(ResilientProvider::new(
                Box::new(arxiv_inner),
                arxiv_interval,
                arxiv_cb,
                config.resilience.clone(),
            ));

            // OpenAlex
            let open_alex_inner = OpenAlexProvider::new(&config.providers.openalex)
                .context("Failed to create OpenAlex provider")?;
            let open_alex_interval =
                Duration::from_millis(config.providers.openalex.rate_limit_interval_ms);
            let open_alex_cb = new_circuit_breaker(&config.resilience);
            let openalex: Box<dyn PaperProvider> = Box::new(ResilientProvider::new(
                Box::new(open_alex_inner),
                open_alex_interval,
                open_alex_cb,
                config.resilience.clone(),
            ));

            let timeout = Duration::from_secs(30);
            let aggregate = AggregateProvider::new(vec![arxiv, openalex], timeout);

            Ok(Box::new(aggregate))
        }
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
    stage: Box<dyn hs_style::reporter::StageHandle>,
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
