use std::sync::Arc;

use async_trait::async_trait;

use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use crate::ports::provider::{doi_search_result, PaperProvider};
use crate::resilience::guard::Guard;

/// A provider whose every search and DOI lookup runs under a shared
/// [`Guard`] (rate limit, circuit breaker, retry).
///
/// Both halves are `Arc`s on purpose: cloning the `Arc<ResilientProvider>`
/// or building a second wrapper over the same `Arc<Guard>` (as
/// [`crate::providers::set::ProviderSet`] does for Semantic Scholar's
/// citation-graph calls) shares one limiter and one breaker. Constructing a
/// fresh `Guard` per call — what the CLI and MCP used to do — shares nothing,
/// which is how an MCP process hammered a provider that was already 429ing.
pub struct ResilientProvider {
    inner: Arc<dyn PaperProvider>,
    guard: Arc<Guard>,
}

impl ResilientProvider {
    pub fn new(inner: Arc<dyn PaperProvider>, guard: Arc<Guard>) -> Self {
        Self { inner, guard }
    }
}

#[async_trait]
impl PaperProvider for ResilientProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn priority(&self) -> u8 {
        self.inner.priority()
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        self.inner.supported_search_types()
    }

    fn supports_offset(&self) -> bool {
        self.inner.supports_offset()
    }

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        if matches!(query.search_type, SearchType::DOI) {
            let paper = self.get_by_doi(&query.query).await?;
            return Ok(doi_search_result(self.name(), paper));
        }
        let inner = &self.inner;
        let mut result = self
            .guard
            .run(|| async move { inner.search_by_query(query).await })
            .await?;
        // No provider filters by citation count itself, so a single-provider
        // search honours `min_citations` here (an aggregate strips it from
        // the member query and applies it once, to the merged papers). A
        // paper whose count the provider does not report is kept.
        if let Some(min) = query.min_citations {
            result
                .papers
                .retain(|paper| paper.cited_by_count.is_none_or(|count| count >= min));
        }
        Ok(result)
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let inner = &self.inner;
        self.guard
            .run(|| async move { inner.get_by_doi(doi).await })
            .await
    }

    fn note_timeout(&self) {
        self.guard.record_timeout();
    }

    async fn health_check(&self) -> Result<(), PaperError> {
        self.inner.health_check().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use super::*;
    use crate::resilience::config::ResilienceConfig;

    /// A provider whose `get_by_doi` replies from a script and records when
    /// it was called.
    struct Scripted {
        calls: AtomicUsize,
        at: Mutex<Vec<Instant>>,
        reply: Box<dyn Fn(usize) -> Result<Option<Paper>, PaperError> + Send + Sync>,
    }

    impl Scripted {
        fn new(
            reply: impl Fn(usize) -> Result<Option<Paper>, PaperError> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                at: Mutex::new(Vec::new()),
                reply: Box::new(reply),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PaperProvider for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn supported_search_types(&self) -> Vec<SearchType> {
            vec![]
        }
        async fn search_by_query(&self, _q: &SearchQuery) -> Result<SearchResult, PaperError> {
            Err(PaperError::ProviderUnavailable("unused".into()))
        }
        async fn get_by_doi(&self, _doi: &str) -> Result<Option<Paper>, PaperError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.at.lock().push(Instant::now());
            (self.reply)(n)
        }
    }

    fn resilience(threshold: u32) -> ResilienceConfig {
        ResilienceConfig {
            cb_failure_threshold: threshold,
            cb_initial_backoff_secs: 1,
            cb_max_backoff_secs: 1,
            retry_max_attempts: 0, // one attempt per call: failures are not masked by retries
            retry_min_backoff_ms: 1,
            retry_max_backoff_secs: 1,
        }
    }

    fn guard(interval_ms: u64, cfg: &ResilienceConfig) -> Arc<Guard> {
        Arc::new(Guard::new("scripted", Duration::from_millis(interval_ms), cfg).unwrap())
    }

    fn down() -> PaperError {
        PaperError::ProviderUnavailable("503".into())
    }

    #[tokio::test]
    async fn the_breaker_opens_after_n_consecutive_failures_and_short_circuits() {
        let inner = Scripted::new(|_| Err(down()));
        let p = ResilientProvider::new(inner.clone(), guard(1, &resilience(3)));

        for i in 0..3 {
            let err = p.get_by_doi("10.1/x").await.unwrap_err();
            assert!(
                matches!(err, PaperError::ProviderUnavailable(_)),
                "call {i}: {err}"
            );
        }
        assert_eq!(inner.calls(), 3);

        // Open: rejected without touching the provider.
        for _ in 0..5 {
            let err = p.get_by_doi("10.1/x").await.unwrap_err();
            assert!(matches!(err, PaperError::CircuitBreakerOpen(_)), "{err}");
        }
        assert_eq!(
            inner.calls(),
            3,
            "an open breaker must not call the provider"
        );
    }

    #[tokio::test]
    async fn the_breaker_half_opens_after_its_backoff_and_closes_on_success() {
        // Fail 3x (opens), then recover.
        let inner = Scripted::new(|n| if n < 3 { Err(down()) } else { Ok(None) });
        let p = ResilientProvider::new(inner.clone(), guard(1, &resilience(3)));
        for _ in 0..3 {
            let _ = p.get_by_doi("10.1/x").await;
        }
        assert!(matches!(
            p.get_by_doi("10.1/x").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));

        // failsafe's backoff is whole seconds (>= 1 s).
        tokio::time::sleep(Duration::from_millis(1200)).await;

        // Half-open probe succeeds -> closed again: calls flow.
        assert!(p.get_by_doi("10.1/x").await.unwrap().is_none());
        assert!(p.get_by_doi("10.1/x").await.unwrap().is_none());
        assert_eq!(inner.calls(), 5);
    }

    #[tokio::test]
    async fn a_success_resets_the_consecutive_failure_count() {
        let inner = Scripted::new(|n| if n % 3 == 2 { Ok(None) } else { Err(down()) });
        let p = ResilientProvider::new(inner.clone(), guard(1, &resilience(3)));
        // fail, fail, ok, fail, fail, ok ... never 3 in a row.
        for _ in 0..9 {
            let _ = p.get_by_doi("10.1/x").await;
        }
        assert_eq!(inner.calls(), 9, "the breaker must never have opened");
    }

    #[tokio::test]
    async fn permanent_errors_do_not_count_against_the_provider() {
        let inner = Scripted::new(|_| Err(PaperError::NotFound("10.1/x".into())));
        let p = ResilientProvider::new(inner.clone(), guard(1, &resilience(2)));
        for _ in 0..10 {
            let err = p.get_by_doi("10.1/x").await.unwrap_err();
            assert!(matches!(err, PaperError::NotFound(_)), "{err}");
        }
        assert_eq!(inner.calls(), 10);
    }

    #[tokio::test]
    async fn rate_limit_responses_count_toward_opening_the_breaker() {
        let inner = Scripted::new(|_| {
            Err(PaperError::RateLimited {
                provider: "scripted".into(),
                retry_after: None,
            })
        });
        let p = ResilientProvider::new(inner.clone(), guard(1, &resilience(2)));
        let _ = p.get_by_doi("10.1/x").await;
        let _ = p.get_by_doi("10.1/x").await;
        assert!(matches!(
            p.get_by_doi("10.1/x").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
        assert_eq!(inner.calls(), 2);
    }

    #[tokio::test]
    async fn transient_failures_are_retried_inside_one_breaker_observation() {
        // Two blips then success: one logical call, recorded as one success,
        // so a threshold of 1 does not trip.
        let inner = Scripted::new(|n| if n < 2 { Err(down()) } else { Ok(None) });
        let cfg = ResilienceConfig {
            retry_max_attempts: 3,
            ..resilience(1)
        };
        let p = ResilientProvider::new(inner.clone(), guard(1, &cfg));
        assert!(p.get_by_doi("10.1/x").await.unwrap().is_none());
        assert_eq!(inner.calls(), 3);
        assert!(p.get_by_doi("10.1/x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn two_callers_of_one_shared_provider_share_one_rate_limiter() {
        let inner = Scripted::new(|_| Ok(None));
        let shared = Arc::new(ResilientProvider::new(
            inner.clone(),
            guard(300, &resilience(3)),
        ));

        // Two independent "requests" (as two MCP calls would be), concurrent.
        let a = shared.clone();
        let b = shared.clone();
        let (ra, rb) = tokio::join!(a.get_by_doi("10.1/a"), b.get_by_doi("10.1/b"));
        ra.unwrap();
        rb.unwrap();

        let mut at = inner.at.lock().clone();
        at.sort();
        assert_eq!(at.len(), 2);
        let gap = at[1].duration_since(at[0]);
        assert!(
            gap >= Duration::from_millis(250),
            "second call must wait for the shared limiter, gap was {gap:?}"
        );
    }

    #[tokio::test]
    async fn separately_built_providers_do_not_share_limiter_state() {
        // The contrast that motivates sharing: two guards, no spacing.
        let inner = Scripted::new(|_| Ok(None));
        let a = ResilientProvider::new(inner.clone(), guard(2000, &resilience(3)));
        let b = ResilientProvider::new(inner.clone(), guard(2000, &resilience(3)));
        let start = Instant::now();
        let (ra, rb) = tokio::join!(a.get_by_doi("10.1/a"), b.get_by_doi("10.1/b"));
        ra.unwrap();
        rb.unwrap();
        assert!(start.elapsed() < Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn one_guard_shared_by_two_wrappers_shares_the_breaker() {
        // Search and citation-graph traffic for one provider use two wrappers
        // over one guard: failures on either open the breaker for both.
        let inner = Scripted::new(|_| Err(down()));
        let g = guard(1, &resilience(2));
        let search = ResilientProvider::new(inner.clone(), g.clone());
        let graph = ResilientProvider::new(inner.clone(), g);
        let _ = search.get_by_doi("10.1/x").await;
        let _ = graph.get_by_doi("10.1/x").await;
        assert!(matches!(
            search.get_by_doi("10.1/x").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
        assert!(matches!(
            graph.get_by_doi("10.1/x").await,
            Err(PaperError::CircuitBreakerOpen(_))
        ));
    }

    #[test]
    fn invalid_resilience_settings_are_errors_not_panics() {
        assert!(Guard::new("x", Duration::ZERO, &ResilienceConfig::default()).is_err());
        let bad = ResilienceConfig {
            cb_initial_backoff_secs: 0,
            ..ResilienceConfig::default()
        };
        assert!(Guard::new("x", Duration::from_millis(10), &bad).is_err());
        let inverted = ResilienceConfig {
            cb_initial_backoff_secs: 9,
            cb_max_backoff_secs: 3,
            ..ResilienceConfig::default()
        };
        assert!(Guard::new("x", Duration::from_millis(10), &inverted).is_err());
    }

    /// A provider whose search returns papers with the given citation counts.
    struct Cited(Vec<Option<u64>>);

    #[async_trait]
    impl PaperProvider for Cited {
        fn name(&self) -> &'static str {
            "cited"
        }
        fn supported_search_types(&self) -> Vec<SearchType> {
            vec![]
        }
        async fn search_by_query(&self, _q: &SearchQuery) -> Result<SearchResult, PaperError> {
            let papers = self
                .0
                .iter()
                .enumerate()
                .map(|(i, count)| Paper {
                    id: format!("p{i}"),
                    title: format!("Paper number {i}"),
                    authors: vec![],
                    abstract_text: None,
                    publication_date: None,
                    doi: None,
                    download_urls: vec![],
                    cited_by_count: *count,
                    source: "cited".into(),
                })
                .collect::<Vec<_>>();
            Ok(SearchResult {
                total_results: papers.len(),
                papers,
                next_offset: None,
                provider: "cited".into(),
                provider_failures: vec![],
            })
        }
    }

    #[tokio::test]
    async fn a_single_provider_search_honours_min_citations() {
        // `--provider openalex --min-citations 100` used to ignore the flag:
        // only the aggregate applied it.
        let p = ResilientProvider::new(
            Arc::new(Cited(vec![Some(500), Some(5), None, Some(100)])),
            guard(1, &resilience(3)),
        );
        let mut query = SearchQuery {
            query: "anything".into(),
            search_type: SearchType::Keywords,
            max_results: 10,
            offset: 0,
            date_filter: None,
            sort_by: crate::models::SortBy::Relevance,
            min_citations: Some(100),
        };

        let ids = |r: SearchResult| r.papers.into_iter().map(|p| p.id).collect::<Vec<_>>();
        // The count at the bound passes; a paper with no reported count is kept.
        assert_eq!(
            ids(p.search_by_query(&query).await.unwrap()),
            ["p0", "p2", "p3"]
        );

        query.min_citations = None;
        assert_eq!(ids(p.search_by_query(&query).await.unwrap()).len(), 4);
    }
}
