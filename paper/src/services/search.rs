// Aggregating search service — fan-out, dedup, merge, rank
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::aggregation::{dedup, merge, quality, ranking};
use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use crate::ports::provider::PaperProvider;

/// Optional callback fired when each provider completes during aggregate search.
pub type OnProviderDone = Arc<dyn Fn(&str) + Send + Sync>;

pub struct AggregateProvider {
    providers: Vec<Box<dyn PaperProvider>>,
    timeout: Duration,
    on_provider_done: Option<OnProviderDone>,
}

impl AggregateProvider {
    pub fn new(providers: Vec<Box<dyn PaperProvider>>, timeout: Duration) -> Self {
        Self {
            providers,
            timeout,
            on_provider_done: None,
        }
    }

    /// Set a callback that fires each time a provider completes (with its name).
    pub fn on_provider_done(mut self, cb: OnProviderDone) -> Self {
        self.on_provider_done = Some(cb);
        self
    }

    /// Number of providers in this aggregate.
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }
}

#[async_trait]
impl PaperProvider for AggregateProvider {
    fn name(&self) -> &'static str {
        "all providers"
    }

    fn priority(&self) -> u8 {
        0
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        self.providers
            .iter()
            .flat_map(|p| p.supported_search_types())
            .collect()
    }

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        use futures::stream::{FuturesUnordered, StreamExt};

        // Fan out to all providers with timeout, collect as each completes
        let mut futs: FuturesUnordered<_> = self
            .providers
            .iter()
            .map(|p| {
                let timeout = self.timeout;
                async move {
                    let start = Instant::now();
                    let result = tokio::time::timeout(timeout, p.search_by_query(query)).await;
                    let elapsed = start.elapsed();
                    (p.name(), result, elapsed)
                }
            })
            .collect();

        let mut source_results: Vec<(String, Vec<Paper>)> = Vec::new();
        let mut total_results: usize = 0;

        while let Some((name, result, _elapsed)) = futs.next().await {
            if let Some(ref cb) = self.on_provider_done {
                cb(name);
            }
            match result {
                Ok(Ok(sr)) => {
                    total_results += sr.total_results;
                    source_results.push((name.to_string(), sr.papers));
                }
                Ok(Err(e)) => {
                    tracing::warn!(provider = %name, error = %e, "provider failed");
                }
                Err(_) => {
                    tracing::warn!(provider = %name, "provider timed out");
                }
            }
        }

        if source_results.is_empty() {
            return Err(PaperError::ProviderUnavailable(
                "All providers failed or timed out".to_string(),
            ));
        }

        // Dedup --> merge --> rank
        let (groups, _stats) = dedup::deduplicate(source_results);
        let merged: Vec<Paper> = groups.iter().map(merge::merge_group).collect();
        let ranked = ranking::rank_papers(&groups, merged, &query.query);
        let ranked = quality::filter_quality(ranked);
        let ranked: Vec<_> = if let Some(min) = query.min_citations {
            ranked
                .into_iter()
                .filter(|rp| match rp.paper.cited_by_count {
                    Some(count) => count >= min,
                    None => true, // Unknown citation count, keep the paper
                })
                .collect()
        } else {
            ranked
        };

        // Convert back to Papers, truncate to max_results
        let papers: Vec<Paper> = ranked
            .into_iter()
            .take(query.max_results)
            .map(|rp| rp.paper)
            .collect();

        Ok(SearchResult {
            papers,
            total_results,
            next_offset: None,
            provider: String::from("all providers"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| {
                let timeout = self.timeout;
                async move { tokio::time::timeout(timeout, p.get_by_doi(doi)).await }
            })
            .collect();

        let results = futures::future::join_all(futures).await;

        let papers: Vec<Paper> = results
            .into_iter()
            .filter_map(|r| match r {
                Ok(Ok(Some(p))) => Some(p),
                _ => None,
            })
            .collect();

        if papers.is_empty() {
            return Ok(None);
        }

        if papers.len() == 1 {
            return Ok(Some(papers.into_iter().next().unwrap()));
        }

        // Multiple providers found it -- merge
        let source_results: Vec<(String, Vec<Paper>)> = papers
            .into_iter()
            .map(|p| {
                let source = p.source.clone();
                (source, vec![p])
            })
            .collect();

        let (groups, _) = dedup::deduplicate(source_results);
        let merged = merge::merge_group(&groups[0]);

        Ok(Some(merged))
    }
}
