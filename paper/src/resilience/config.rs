use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for resilience patterns
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResilienceConfig {
    /// Circuit breaker: initial backoff duration
    pub cb_initial_backoff_secs: u64,

    /// Circuit breaker: maximum backoff u64
    pub cb_max_backoff_secs: u64,

    /// Circuit breaker: consecutive failures before opening
    pub cb_failure_threshold: u32,

    /// Retry: maximum number of attempts
    pub retry_max_attempts: usize,

    /// Retry: minimum number of attempts
    pub retry_min_backoff_ms: u64,

    /// Retry: maximum backoff u64
    pub retry_max_backoff_secs: u64,
}

impl ResilienceConfig {
    pub fn cb_initial_backoff(&self) -> Duration {
        Duration::from_secs(self.cb_initial_backoff_secs)
    }
    pub fn cb_max_backoff(&self) -> Duration {
        Duration::from_secs(self.cb_max_backoff_secs)
    }
    pub fn retry_max_backoff(&self) -> Duration {
        Duration::from_secs(self.retry_max_backoff_secs)
    }
    pub fn retry_min_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_min_backoff_ms)
    }
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            cb_initial_backoff_secs: 10_u64,
            cb_max_backoff_secs: 60_u64,
            cb_failure_threshold: 3,
            retry_max_attempts: 5,
            retry_min_backoff_ms: 100_u64,
            retry_max_backoff_secs: 30_u64,
        }
    }
}
