//! One provider's rate limiter, circuit breaker and retry policy, built once
//! and shared.
//!
//! A [`Guard`] is the unit of *shared state*: every call to the provider it
//! guards — search, DOI lookup, citation-graph fetch, a download resolver —
//! goes through the same limiter (so concurrent callers are spaced out
//! together) and the same breaker (so consecutive failures anywhere open it
//! for everyone). Build one per provider per process and hand out `Arc`s; see
//! [`crate::providers::set::ProviderSet`].

use std::future::Future;
use std::time::Duration;

use failsafe::futures::CircuitBreaker as _;

use crate::error::{ErrorCategory, PaperError};
use crate::resilience::circuit_breaker::{new_circuit_breaker, ProviderBreaker};
use crate::resilience::config::ResilienceConfig;
use crate::resilience::rate_limiter::ProviderRateLimiter;
use crate::resilience::retry::retry_with_backoff;

pub struct Guard {
    name: &'static str,
    limiter: ProviderRateLimiter,
    breaker: ProviderBreaker,
    retry: ResilienceConfig,
}

impl Guard {
    /// `rate_limit_interval` is the minimum spacing between two requests;
    /// zero (and an invalid `config`) is an `Err`, never a panic.
    pub fn new(
        name: &'static str,
        rate_limit_interval: Duration,
        config: &ResilienceConfig,
    ) -> Result<Self, PaperError> {
        Ok(Self {
            name,
            limiter: ProviderRateLimiter::new(rate_limit_interval)?,
            breaker: new_circuit_breaker(config)?,
            retry: config.clone(),
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Run `op` under the guard.
    ///
    /// * The breaker is consulted first: while it is open the call is
    ///   rejected with [`PaperError::CircuitBreakerOpen`] without running
    ///   `op`.
    /// * Every attempt — including retries — first waits for the shared
    ///   rate limiter.
    /// * Transient failures are retried with backoff; the *final* outcome of
    ///   the whole call is recorded on the breaker: success closes/resets it,
    ///   a transient or rate-limit failure counts toward opening it. A
    ///   permanent error (not found, bad input, parse error) says nothing
    ///   about the provider's health and is not counted.
    pub async fn run<T, F, Fut>(&self, mut op: F) -> Result<T, PaperError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, PaperError>>,
    {
        let attempts = retry_with_backoff(&self.retry, || {
            let call = op();
            async {
                self.limiter.acquire().await;
                call.await
            }
        });

        match self
            .breaker
            .call_with(counts_against_provider, attempts)
            .await
        {
            Ok(value) => Ok(value),
            Err(failsafe::Error::Inner(err)) => Err(err),
            Err(failsafe::Error::Rejected) => {
                Err(PaperError::CircuitBreakerOpen(self.name.to_string()))
            }
        }
    }
}

fn counts_against_provider(err: &PaperError) -> bool {
    matches!(
        err.category(),
        ErrorCategory::Transient | ErrorCategory::RateLimited
    )
}
