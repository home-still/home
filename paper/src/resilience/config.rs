use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::PaperError;

/// Configuration for resilience patterns
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResilienceConfig {
    /// Circuit breaker: initial backoff duration (>= 1)
    pub cb_initial_backoff_secs: u64,

    /// Circuit breaker: maximum backoff (>= cb_initial_backoff_secs)
    pub cb_max_backoff_secs: u64,

    /// Circuit breaker: consecutive failures before opening (>= 1)
    pub cb_failure_threshold: u32,

    /// Retry: maximum number of retries after the first attempt
    pub retry_max_attempts: usize,

    /// Retry: minimum backoff between attempts, in milliseconds (>= 1)
    pub retry_min_backoff_ms: u64,

    /// Retry: maximum backoff (>= retry_min_backoff_ms)
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

    /// Reject values the circuit breaker (`failsafe` asserts on sub-second or
    /// inverted backoffs) and the retry policy cannot honour. Run by
    /// `Config::load` and by every constructor that builds a guard.
    pub fn validate(&self) -> Result<(), PaperError> {
        let fail = |msg: &str| Err(PaperError::InvalidInput(format!("paper.resilience.{msg}")));
        if self.cb_failure_threshold == 0 {
            return fail("cb_failure_threshold must be at least 1");
        }
        if self.cb_initial_backoff_secs == 0 {
            return fail("cb_initial_backoff_secs must be at least 1");
        }
        if self.cb_max_backoff_secs < self.cb_initial_backoff_secs {
            return fail("cb_max_backoff_secs must be >= cb_initial_backoff_secs");
        }
        if self.retry_min_backoff_ms == 0 {
            return fail("retry_min_backoff_ms must be at least 1");
        }
        if self.retry_max_backoff() < self.retry_min_backoff() {
            return fail("retry_max_backoff_secs must be >= retry_min_backoff_ms");
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        ResilienceConfig::default().validate().unwrap();
    }

    #[test]
    fn values_that_would_panic_or_stall_are_rejected() {
        let base = ResilienceConfig::default();
        let cases = [
            ResilienceConfig {
                cb_failure_threshold: 0,
                ..base.clone()
            },
            ResilienceConfig {
                cb_initial_backoff_secs: 0,
                ..base.clone()
            },
            ResilienceConfig {
                cb_initial_backoff_secs: 30,
                cb_max_backoff_secs: 10,
                ..base.clone()
            },
            ResilienceConfig {
                retry_min_backoff_ms: 0,
                ..base.clone()
            },
            ResilienceConfig {
                retry_min_backoff_ms: 5_000,
                retry_max_backoff_secs: 1,
                ..base.clone()
            },
        ];
        for cfg in cases {
            assert!(
                matches!(cfg.validate(), Err(PaperError::InvalidInput(_))),
                "{cfg:?}"
            );
        }
    }

    #[test]
    fn a_partial_resilience_section_keeps_the_other_defaults() {
        // Without `#[serde(default)]` this was a "missing field" error, which
        // also made a single `...RESILIENCE_CB_FAILURE_THRESHOLD` env override
        // fail the whole load.
        let cfg: ResilienceConfig = serde_yaml_ng::from_str("cb_failure_threshold: 7").unwrap();
        assert_eq!(cfg.cb_failure_threshold, 7);
        assert_eq!(cfg.cb_initial_backoff_secs, 10);
    }
}
