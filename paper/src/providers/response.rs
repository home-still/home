use crate::error::PaperError;
use serde::de::DeserializeOwned;

/// Send a request, and on a 429 retry up to two more times with growing
/// jittered backoff. Returns the final response (which may itself be a 429
/// — `check_response` then maps it to `RateLimited`, whose error message is
/// phrased as a retry-directive an LLM agent will naturally act on).
///
/// Anonymous-tier providers (notably Semantic Scholar) return 429s on the
/// first call when the shared bucket is exhausted, often with no
/// `Retry-After` header. Two bounded retries (≈2s + ≈5s) silently absorb
/// the common case where the bucket refills within a few seconds; only the
/// pathological "bucket stays empty" cases reach the caller.
pub async fn send_with_429_retry(
    builder: reqwest::RequestBuilder,
    provider: &str,
) -> Result<reqwest::Response, PaperError> {
    // Backoff delays applied between attempts. `len() + 1` total attempts.
    const RETRY_DELAYS_MS: [u64; 2] = [2000, 5000];

    let mut current = Some(builder);

    for (attempt, &base_ms) in RETRY_DELAYS_MS.iter().enumerate() {
        let req = current
            .take()
            .expect("loop invariant: `current` is Some at the top of each iteration");
        // Reserve a clone for the next attempt before we consume `req`.
        let next = req.try_clone();

        let response = req.send().await?;
        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(response);
        }
        let Some(retry_req) = next else {
            // Body wasn't cloneable; surface the 429 for `check_response` to map.
            return Ok(response);
        };
        let header_retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let sleep = header_retry_after.unwrap_or_else(|| {
            let jitter_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| (d.subsec_millis() as u64) % 1000)
                .unwrap_or(0);
            std::time::Duration::from_millis(base_ms + jitter_ms)
        });
        tracing::info!(
            provider = provider,
            attempt = attempt + 1,
            sleep_ms = sleep.as_millis() as u64,
            "429 received, retrying"
        );
        drop(response);
        tokio::time::sleep(sleep).await;
        current = Some(retry_req);
    }

    // Final attempt — no further retries; whatever comes back goes to the caller.
    let req = current.expect("loop invariant: `current` is Some after the retry loop");
    Ok(req.send().await?)
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
