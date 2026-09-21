use super::config::ResilienceConfig;
use crate::error::{ErrorCategory, PaperError};
use backon::{ExponentialBuilder, Retryable};

/// Retries an async operation with exponential backoff.
///
/// Only retries on transient errors (network/5xx). `RateLimited` is
/// intentionally NOT retried here: per-request 429 backoff already happens
/// inside `providers::response::send_with_429_retry`, and stacking the
/// outer retry on top of it just burns the per-provider 30s timeout
/// spinning on the same exhausted bucket.
pub async fn retry_with_backoff<F, Fut, T>(
    config: &ResilienceConfig,
    operation: F,
) -> Result<T, PaperError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, PaperError>>,
{
    let backoff = ExponentialBuilder::default()
        .with_max_times(config.retry_max_attempts)
        .with_min_delay(config.retry_min_backoff())
        .with_max_delay(config.retry_max_backoff())
        .with_jitter();

    operation
        .retry(backoff)
        .when(|err| matches!(err.category(), ErrorCategory::Transient))
        .await
}
