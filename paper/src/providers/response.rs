use crate::error::PaperError;
use serde::de::DeserializeOwned;

/// Send a request, and on a 429 with no `Retry-After` header retry exactly
/// once after a short jittered sleep. Returns the final response (which may
/// itself be a 429 — `check_response` then maps it to `RateLimited` as today).
///
/// Anonymous-tier providers (notably Semantic Scholar) return 429s on the
/// very first call when the shared bucket is exhausted, with no retry-after
/// header. A single bounded retry turns most of those into success without
/// pretending we have a real rate-limit budget.
pub async fn send_with_429_retry(
    builder: reqwest::RequestBuilder,
    provider: &str,
) -> Result<reqwest::Response, PaperError> {
    let cloned = builder.try_clone();
    let response = builder.send().await?;
    if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Ok(response);
    }
    let header_retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);
    let Some(retry_builder) = cloned else {
        return Ok(response);
    };
    let sleep = header_retry_after.unwrap_or_else(|| {
        let jitter_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.subsec_millis() as u64) % 1500)
            .unwrap_or(0);
        std::time::Duration::from_millis(1500 + jitter_ms)
    });
    tracing::info!(
        provider = provider,
        sleep_ms = sleep.as_millis() as u64,
        "429 received, retrying once"
    );
    drop(response);
    tokio::time::sleep(sleep).await;
    Ok(retry_builder.send().await?)
}

pub fn check_response(response: &reqwest::Response, provider: &str) -> Result<(), PaperError> {
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        return Err(PaperError::RateLimited {
            provider: provider.to_string(),
            retry_after,
        });
    } else if !response.status().is_success() {
        return Err(PaperError::ProviderUnavailable(format!(
            "{} returned {}",
            provider,
            response.status()
        )));
    }
    Ok(())
}

/// Deserialize a JSON response body, capturing the raw bytes and `Content-Type`
/// so parse failures produce a diagnosable error instead of a bare
/// "error decoding response body".
pub async fn parse_json_or_log<T: DeserializeOwned>(
    response: reqwest::Response,
    provider: &str,
) -> Result<T, PaperError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| PaperError::ParseError(format!("{} body read failed: {}", provider, e)))?;

    match serde_json::from_slice::<T>(&bytes) {
        Ok(v) => Ok(v),
        Err(e) => {
            let preview = String::from_utf8_lossy(&bytes);
            let preview: String = preview.chars().take(512).collect();
            tracing::warn!(
                provider = provider,
                content_type = %content_type,
                body_preview = %preview,
                "failed to parse response body"
            );
            Err(PaperError::ParseError(format!(
                "Failed to parse {} response ({}; content-type={}): body[..512]={}",
                provider, e, content_type, preview
            )))
        }
    }
}
