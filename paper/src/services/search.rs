// Aggregating search service — fan-out, dedup, merge, rank
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::aggregation::{dedup, merge, ranking};
use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use crate::ports::provider::PaperProvider;

pub struct AggregateProvider {
    providers: Vec<Box<dyn PaperProvider>>,
    timeout: Duration,
}

impl AggregateProvider {
    pub fn new(providers: Vec<Box<dyn PaperProvider>>, timeout: Duration) -> Self {
        Self { providers, timeout }
    }
}

#[async_trait]
impl PaperProvider for AggregateProvider {
    fn name(&self) -> &'static str {
        "aggregate"
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

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        // Fan out to all providers with timeout
        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| {
                let timeout = self.timeout;
                async move {
                    let start = Instant::now();
                    let result = tokio::time::timeout(timeout, p.search(query)).await;
                    let elapased = start.elapsed();
                    (p.name(), result, elapased)
                }
            })
            .collect();

        let results = futures::future::join_all(futures).await;

        // Collect successful results
        let mut source_results: Vec<(String, Vec<Paper>)> = Vec::new();
        let mut total_results: usize = 0;

        for (name, result, _elapsed) in results {
            match result {
                Ok(Ok(sr)) => {
                    total_results += sr.total_results;
                    source_results.push((name.to_string(), sr.papers));
                }
                Ok(Err(_e)) => {} // provider error - skip
                Err(_) => {}
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
        let ranked = ranking::rank_papers(&groups, merged);

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
            provider: String::from("aggregate"),
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
