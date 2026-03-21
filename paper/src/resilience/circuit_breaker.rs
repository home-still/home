use crate::resilience::config::ResilienceConfig;
use failsafe::{backoff, failure_policy, Config};

/// Creates a new circuit breaker configured for provider resilience.
///
/// Configuration:
/// - Exponential backoff from config.cb_initial_backoff to config.cb_max_backoff
/// - Opens after config.cb_failure_threshold consecutive failures
pub fn new_circuit_breaker(config: &ResilienceConfig) -> impl failsafe::CircuitBreaker + Clone {
    let backoff = backoff::exponential(config.cb_initial_backoff(), config.cb_max_backoff());
    let policy = failure_policy::consecutive_failures(config.cb_failure_threshold, backoff);
    Config::new().failure_policy(policy).build()
}
