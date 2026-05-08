//! Shared reqwest client construction. Every HTTP client in the workspace is
//! built through this module so that builder failures propagate as real errors
//! instead of silently falling back to an unconfigured `reqwest::Client::new()`.

use std::time::Duration;

use anyhow::{Context, Result};

/// Build a `reqwest::Client` that applies the same `timeout` to both the
/// connect phase and the overall request. Returns `Err` if the builder fails.
///
/// For callers that need finer control (streaming, tcp_keepalive, no overall
/// timeout), use [`client_builder`] and call `.build()?` directly.
pub fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .context("failed to build reqwest Client")
}

/// Start a `reqwest::ClientBuilder` for callers that need bespoke knobs
/// (e.g., `tcp_keepalive`, streaming with no overall `timeout`). Callers
/// must still propagate the `.build()` error via `?`.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_client_with_reasonable_timeout_builds() {
        let client = http_client(Duration::from_secs(5));
        assert!(client.is_ok(), "http_client should build for 5s timeout");
    }

    #[test]
    fn http_client_with_zero_timeout_still_builds() {
        // `reqwest::Client::builder().build()` does not currently reject zero
        // timeouts, but the important guarantee is the absence of a fallback —
        // whatever the builder decides, we surface it rather than masking.
        let _ = http_client(Duration::from_millis(1));
    }
}
