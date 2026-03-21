use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use std::num::NonZeroU32;
use std::time::Duration;

pub struct ProviderRateLimiter {
    limiter: DefaultDirectRateLimiter,
}

impl ProviderRateLimiter {
    pub fn new(interval: Duration) -> Self {
        let quota = Quota::with_period(interval)
            .expect("interval must be non-zero")
            .allow_burst(NonZeroU32::new(1).unwrap());

        Self {
            limiter: RateLimiter::direct(quota),
        }
    }

    pub async fn acquire(&self) {
        self.limiter.until_ready().await;
    }
}
