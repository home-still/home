use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use std::num::NonZeroU32;
use std::time::Duration;

use crate::error::PaperError;

pub struct ProviderRateLimiter {
    limiter: DefaultDirectRateLimiter,
}

impl ProviderRateLimiter {
    /// One request per `interval`, no bursts. A zero interval is an `Err`
    /// (governor cannot express "no limit" and panicked on it).
    pub fn new(interval: Duration) -> Result<Self, PaperError> {
        let quota = Quota::with_period(interval)
            .ok_or_else(|| {
                PaperError::InvalidInput("rate_limit_interval_ms must be at least 1".to_string())
            })?
            .allow_burst(NonZeroU32::MIN);

        Ok(Self {
            limiter: RateLimiter::direct(quota),
        })
    }

    pub async fn acquire(&self) {
        self.limiter.until_ready().await;
    }
}
