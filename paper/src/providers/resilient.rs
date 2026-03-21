use async_trait::async_trait;
use std::time::Duration;

use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use crate::ports::provider::PaperProvider;
use crate::resilience::config::ResilienceConfig;
use crate::resilience::rate_limiter::ProviderRateLimiter;
use crate::resilience::retry::retry_with_backoff;

pub struct ResilientProvider<CB: failsafe::CircuitBreaker> {
    inner: Box<dyn PaperProvider>,
    rate_limiter: ProviderRateLimiter,
    circuit_breaker: CB,
    resilience_config: ResilienceConfig,
}

impl<CB: failsafe::CircuitBreaker + Clone> ResilientProvider<CB> {
    pub fn new(
        inner: Box<dyn PaperProvider>,
        rate_limit_interval: Duration,
        circuit_breaker: CB,
        resilience_config: ResilienceConfig,
    ) -> Self {
        Self {
            inner,
            rate_limiter: ProviderRateLimiter::new(rate_limit_interval),
            circuit_breaker,
            resilience_config,
        }
    }
}

#[async_trait]
impl<CB: failsafe::CircuitBreaker + Clone + Send + Sync + 'static> PaperProvider
    for ResilientProvider<CB>
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn priority(&self) -> u8 {
        self.inner.priority()
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        self.inner.supported_search_types()
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        if !self.circuit_breaker.is_call_permitted() {
            return Err(PaperError::CircuitBreakerOpen(String::from(
                self.inner.name(),
            )));
        }

        self.rate_limiter.acquire().await;

        let inner = &self.inner;

        retry_with_backoff(&self.resilience_config, || async {
            inner.search(query).await
        })
        .await
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        if !self.circuit_breaker.is_call_permitted() {
            return Err(PaperError::CircuitBreakerOpen(String::from(
                self.inner.name(),
            )));
        }
        self.rate_limiter.acquire().await;

        let inner = &self.inner;

        retry_with_backoff(&self.resilience_config, || async {
            inner.get_by_doi(doi).await
        })
        .await
    }

    async fn health_check(&self) -> Result<(), PaperError> {
        self.inner.health_check().await
    }
}
