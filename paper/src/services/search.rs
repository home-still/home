// Aggregating search service — fan-out, dedup, merge, rank
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use crate::aggregation::{dedup, merge, quality, ranking, relevance};
use crate::error::PaperError;
use crate::models::{Paper, ProviderFailure, SearchQuery, SearchResult, SearchType};
use crate::ports::provider::PaperProvider;

/// Optional callback fired when each provider completes during aggregate search.
pub type OnProviderDone = Arc<dyn Fn(&str) + Send + Sync>;

pub struct AggregateProvider {
    providers: Vec<Arc<dyn PaperProvider>>,
    timeout: Duration,
    on_provider_done: Option<OnProviderDone>,
}

impl AggregateProvider {
    /// Members are `Arc`s so a long-lived process can build one set of
    /// guarded providers and put the same instances behind every aggregate
    /// (see [`crate::providers::set::ProviderSet`]).
    pub fn new(providers: Vec<Arc<dyn PaperProvider>>, timeout: Duration) -> Self {
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
                    let result = tokio::time::timeout(timeout, p.search_by_query(query)).await;
                    (p.name(), timeout, result)
                }
            })
            .collect();

        let mut source_results: Vec<(String, Vec<Paper>)> = Vec::new();
        let mut total_results: usize = 0;
        let mut failures: Vec<ProviderFailure> = Vec::new();

        while let Some((name, timeout, result)) = futs.next().await {
            if let Some(cb) = &self.on_provider_done {
                cb(name);
            }
            match result {
                Ok(Ok(sr)) => {
                    total_results += sr.total_results;
                    source_results.push((name.to_string(), sr.papers));
                }
                Ok(Err(e)) => {
                    tracing::warn!(provider = %name, error = %e, "provider failed");
                    failures.push(ProviderFailure {
                        provider: name.to_string(),
                        error: e.to_string(),
                    });
                }
                Err(_) => {
                    tracing::warn!(provider = %name, "provider timed out");
                    failures.push(ProviderFailure {
                        provider: name.to_string(),
                        error: timed_out(timeout),
                    });
                }
            }
        }
        // Completion order is nondeterministic; report failures in a stable order.
        failures.sort_by(|a, b| a.provider.cmp(&b.provider));

        if source_results.is_empty() {
            return Err(PaperError::ProvidersFailed {
                succeeded: 0,
                failures,
            });
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

        // When the caller sorts by citations, drop low-relevance papers so the
        // 25% citation weight in the ranking formula doesn't amplify
        // high-citation off-topic hits above the target paper. The title-presence
        // floor is applied HERE (citation-sort only), not inside relevance_score,
        // so the default relevance ranking never demotes an on-topic abstract
        // match that lacks the query terms in its title.
        let ranked: Vec<_> = if matches!(query.sort_by, crate::models::SortBy::Citations) {
            ranked
                .into_iter()
                .filter(|rp| {
                    rp.relevance >= ranking::CITATION_SORT_MIN_RELEVANCE
                        && relevance::passes_citation_title_floor(&query.query, &rp.paper)
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
            provider_failures: failures,
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        // arXiv DOIs (`10.48550/arXiv.*`) are registered with DataCite, not
        // Crossref — Crossref/OpenAlex/Semantic Scholar don't index them, so
        // fanning out is wasted work and returns the wrong answer (empty).
        // Short-circuit to the arXiv provider, which knows how to translate
        // the prefix into an id_list lookup. Its error (or its `None`) is the
        // answer: there is nobody else to ask.
        if crate::providers::downloader::strip_arxiv_doi_prefix(doi).is_some() {
            if let Some(arxiv) = self.providers.iter().find(|p| p.name() == "arxiv") {
                return arxiv.get_by_doi(doi).await;
            }
        }

        let futures: Vec<_> = self
            .providers
            .iter()
            .map(|p| {
                let timeout = self.timeout;
                async move {
                    (
                        p.name(),
                        timeout,
                        tokio::time::timeout(timeout, p.get_by_doi(doi)).await,
                    )
                }
            })
            .collect();

        let mut papers: Vec<Paper> = Vec::new();
        let mut not_found = 0usize;
        let mut failures: Vec<ProviderFailure> = Vec::new();
        for (name, timeout, result) in futures::future::join_all(futures).await {
            match result {
                Ok(Ok(Some(p))) => papers.push(p),
                Ok(Ok(None)) => not_found += 1,
                Ok(Err(e)) => failures.push(ProviderFailure {
                    provider: name.to_string(),
                    error: e.to_string(),
                }),
                Err(_) => failures.push(ProviderFailure {
                    provider: name.to_string(),
                    error: timed_out(timeout),
                }),
            }
        }

        if papers.is_empty() {
            // Not found is an answer only if nobody failed to give one. If
            // any provider errored, timed out or was rate limited, the paper
            // may well exist: say so instead of "no paper found".
            return if failures.is_empty() {
                Ok(None)
            } else {
                Err(PaperError::ProvidersFailed {
                    succeeded: not_found,
                    failures,
                })
            };
        }

        for f in &failures {
            tracing::warn!(provider = %f.provider, error = %f.error, "provider failed during DOI lookup");
        }

        if papers.len() == 1 {
            return Ok(papers.pop());
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

fn timed_out(timeout: Duration) -> String {
    format!("timed out after {timeout:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCategory;

    type Reply<T> = Box<dyn Fn() -> Result<T, PaperError> + Send + Sync>;

    struct Fake {
        name: &'static str,
        delay: Duration,
        search: Reply<SearchResult>,
        lookup: Reply<Option<Paper>>,
    }

    #[async_trait]
    impl PaperProvider for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_search_types(&self) -> Vec<SearchType> {
            vec![]
        }
        async fn search_by_query(&self, _q: &SearchQuery) -> Result<SearchResult, PaperError> {
            tokio::time::sleep(self.delay).await;
            (self.search)()
        }
        async fn get_by_doi(&self, _doi: &str) -> Result<Option<Paper>, PaperError> {
            tokio::time::sleep(self.delay).await;
            (self.lookup)()
        }
    }

    fn paper(title: &str, doi: &str) -> Paper {
        Paper {
            id: title.to_string(),
            title: title.to_string(),
            authors: vec![],
            abstract_text: None,
            publication_date: None,
            doi: Some(doi.to_string()),
            download_urls: vec![],
            cited_by_count: None,
            source: "test".to_string(),
        }
    }

    fn rate_limited(provider: &str) -> PaperError {
        PaperError::RateLimited {
            provider: provider.to_string(),
            retry_after: Some(Duration::from_secs(3600)),
        }
    }

    /// A provider that answers with `papers`.
    fn ok(name: &'static str, papers: Vec<Paper>) -> Arc<dyn PaperProvider> {
        let for_search = papers.clone();
        Arc::new(Fake {
            name,
            delay: Duration::ZERO,
            search: Box::new(move || {
                Ok(SearchResult {
                    total_results: for_search.len(),
                    papers: for_search.clone(),
                    next_offset: None,
                    provider: name.to_string(),
                    provider_failures: vec![],
                })
            }),
            lookup: Box::new(move || Ok(papers.first().cloned())),
        })
    }

    /// A provider that fails every call with a rate-limit error.
    fn limited(name: &'static str) -> Arc<dyn PaperProvider> {
        Arc::new(Fake {
            name,
            delay: Duration::ZERO,
            search: Box::new(move || Err(rate_limited(name))),
            lookup: Box::new(move || Err(rate_limited(name))),
        })
    }

    /// A provider that answers "nothing" (a clean not-found / empty result).
    fn empty(name: &'static str) -> Arc<dyn PaperProvider> {
        ok(name, vec![])
    }

    /// A provider slower than the aggregate's timeout.
    fn hung(name: &'static str) -> Arc<dyn PaperProvider> {
        Arc::new(Fake {
            name,
            delay: Duration::from_secs(30),
            search: Box::new(|| unreachable!("timed out first")),
            lookup: Box::new(|| unreachable!("timed out first")),
        })
    }

    fn agg(providers: Vec<Arc<dyn PaperProvider>>) -> AggregateProvider {
        AggregateProvider::new(providers, Duration::from_millis(100))
    }

    fn query() -> SearchQuery {
        SearchQuery {
            query: "transformers".into(),
            search_type: SearchType::Keywords,
            max_results: 10,
            offset: 0,
            date_filter: None,
            sort_by: crate::models::SortBy::Relevance,
            min_citations: None,
        }
    }

    #[tokio::test]
    async fn search_returns_results_and_names_every_provider_that_failed() {
        let a = agg(vec![
            ok("arxiv", vec![paper("Attention", "10.1/a")]),
            limited("semantic_scholar"),
            hung("core"),
        ]);

        let result = a.search_by_query(&query()).await.unwrap();

        // (The quality filter may drop a bare fixture paper; the failures are
        // what this test pins.)
        let failed: Vec<(&str, &str)> = result
            .provider_failures
            .iter()
            .map(|f| (f.provider.as_str(), f.error.as_str()))
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
        // Stable (alphabetical) order regardless of completion order.
        assert_eq!(failed[0].0, "core");
        assert!(failed[0].1.contains("timed out"), "{failed:?}");
        assert_eq!(failed[1].0, "semantic_scholar");
        assert!(failed[1].1.contains("Rate limited"), "{failed:?}");
    }

    #[tokio::test]
    async fn search_with_no_failures_reports_none() {
        let a = agg(vec![
            ok("arxiv", vec![paper("Attention", "10.1/a")]),
            empty("core"),
        ]);
        let result = a.search_by_query(&query()).await.unwrap();
        assert!(result.provider_failures.is_empty());
    }

    #[tokio::test]
    async fn failed_providers_serialize_only_when_present() {
        let clean = agg(vec![ok("arxiv", vec![paper("Attention", "10.1/a")])]);
        let json = serde_json::to_value(clean.search_by_query(&query()).await.unwrap()).unwrap();
        assert!(json.get("provider_failures").is_none(), "{json}");

        let partial = agg(vec![
            ok("arxiv", vec![paper("Attention", "10.1/a")]),
            limited("core"),
        ]);
        let json = serde_json::to_value(partial.search_by_query(&query()).await.unwrap()).unwrap();
        assert_eq!(json["provider_failures"][0]["provider"], "core");
    }

    #[tokio::test]
    async fn search_where_every_provider_failed_is_an_error_listing_each() {
        let a = agg(vec![limited("arxiv"), hung("core"), limited("crossref")]);

        let err = a.search_by_query(&query()).await.unwrap_err();

        let PaperError::ProvidersFailed {
            succeeded,
            failures,
        } = &err
        else {
            panic!("expected ProvidersFailed, got {err}");
        };
        assert_eq!(*succeeded, 0);
        let names: Vec<&str> = failures.iter().map(|f| f.provider.as_str()).collect();
        assert_eq!(names, ["arxiv", "core", "crossref"]);
        let msg = err.to_string();
        for name in names {
            assert!(msg.contains(name), "{name} missing from: {msg}");
        }
        assert!(matches!(err.category(), ErrorCategory::Transient));
    }

    #[tokio::test]
    async fn an_empty_but_successful_search_is_not_an_error() {
        let a = agg(vec![empty("arxiv"), empty("core")]);
        let result = a.search_by_query(&query()).await.unwrap();
        assert!(result.papers.is_empty());
        assert!(result.provider_failures.is_empty());
    }

    #[tokio::test]
    async fn doi_lookup_is_not_found_only_when_every_provider_said_so() {
        let a = agg(vec![empty("arxiv"), empty("crossref")]);
        assert!(a.get_by_doi("10.1/x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn doi_lookup_does_not_say_not_found_when_providers_were_rate_limited() {
        // The old behaviour: Ok(None) — "no such paper" — because one
        // provider was rate limited and the other had no record.
        let a = agg(vec![empty("arxiv"), limited("semantic_scholar")]);

        let err = a.get_by_doi("10.1/x").await.unwrap_err();

        let PaperError::ProvidersFailed {
            succeeded,
            failures,
        } = &err
        else {
            panic!("expected ProvidersFailed, got {err}");
        };
        assert_eq!(*succeeded, 1, "arxiv answered 'not found'");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].provider, "semantic_scholar");
        assert!(err.to_string().contains("unconfirmed"), "{err}");
    }

    #[tokio::test]
    async fn doi_lookup_when_every_provider_timed_out_is_an_error() {
        let a = agg(vec![hung("arxiv"), hung("crossref")]);
        let err = a.get_by_doi("10.1/x").await.unwrap_err();
        assert!(
            matches!(&err, PaperError::ProvidersFailed { succeeded: 0, failures } if failures.len() == 2),
            "{err}"
        );
    }

    #[tokio::test]
    async fn doi_lookup_returns_the_paper_despite_other_providers_failing() {
        let a = agg(vec![
            ok("crossref", vec![paper("Attention", "10.1/x")]),
            limited("semantic_scholar"),
            hung("core"),
        ]);
        let found = a.get_by_doi("10.1/x").await.unwrap().unwrap();
        assert_eq!(found.title, "Attention");
    }

    #[tokio::test]
    async fn arxiv_doi_lookup_surfaces_the_arxiv_providers_failure() {
        // arXiv DOIs go only to arXiv: its error is the answer, not "None".
        let a = agg(vec![
            limited("arxiv"),
            ok("crossref", vec![paper("x", "10.1/x")]),
        ]);
        let err = a.get_by_doi("10.48550/arXiv.2005.11401").await.unwrap_err();
        assert!(matches!(err, PaperError::RateLimited { .. }), "{err}");
    }
}
