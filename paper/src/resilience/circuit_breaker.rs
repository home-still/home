use crate::error::PaperError;
use crate::resilience::config::ResilienceConfig;
use failsafe::{backoff, failure_policy, Config, StateMachine};

/// The breaker every provider guard uses. A concrete type (not
/// `impl CircuitBreaker`) because the async `failsafe::futures::CircuitBreaker`
/// interface — the one that records the outcome of a call — is what
/// [`crate::resilience::guard::Guard`] needs, and clones of a `StateMachine`
/// share state.
pub type ProviderBreaker =
    StateMachine<failure_policy::ConsecutiveFailures<backoff::Exponential>, ()>;

/// Creates a new circuit breaker configured for provider resilience.
///
/// Configuration:
/// - Exponential backoff from config.cb_initial_backoff to config.cb_max_backoff
/// - Opens after config.cb_failure_threshold consecutive failures
///
/// `failsafe` panics on sub-second or inverted backoffs, so the config is
/// validated first and a bad one is an `Err`, not an abort.
pub fn new_circuit_breaker(config: &ResilienceConfig) -> Result<ProviderBreaker, PaperError> {
    config.validate()?;
    let backoff = backoff::exponential(config.cb_initial_backoff(), config.cb_max_backoff());
    let policy = failure_policy::consecutive_failures(config.cb_failure_threshold, backoff);
    Ok(Config::new().failure_policy(policy).build())
}
