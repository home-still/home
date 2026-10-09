use crate::error::PaperError;
use serde::de::DeserializeOwned;
use std::time::Duration;

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
///
/// A server-supplied `Retry-After` is honoured only up to `max_retry_after`:
/// the header is remote text, and `Retry-After: 86400` must not park a
/// request (and the caller's permit) for a day. A longer directive costs one
/// capped sleep, then the next attempt gets the 429 again and the
/// `RateLimited` error — carrying the real `retry_after` — reaches the
/// caller.
pub async fn send_with_429_retry(
    builder: reqwest::RequestBuilder,
    provider: &str,
    max_retry_after: Duration,
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
        let header_retry_after = retry_after(response.headers());
        let sleep = header_retry_after
            .unwrap_or_else(|| {
                let jitter_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.subsec_millis() as u64) % 1000)
                    .unwrap_or(0);
                Duration::from_millis(base_ms + jitter_ms)
            })
            .min(max_retry_after);
        tracing::info!(
            provider = provider,
            attempt = attempt + 1,
            sleep_ms = sleep.as_millis() as u64,
            retry_after_header_secs = header_retry_after.map(|d| d.as_secs()),
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

/// The server's `Retry-After` directive in its delta-seconds form (`120`),
/// whitespace-trimmed. Remote text: anything else (the HTTP-date form, junk,
/// an overflowing number) is "no directive", never an error — the caller then
/// applies its own backoff.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub fn check_response(response: &reqwest::Response, provider: &str) -> Result<(), PaperError> {
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = retry_after(response.headers());
        return Err(PaperError::RateLimited {
            provider: provider.to_string(),
            retry_after,
        });
    } else if !response.status().is_success() {
        return Err(status_error(provider, response.status()));
    }
    Ok(())
}

/// The error for a non-success, non-429 status. A 4xx says the request
/// itself is wrong ([`PaperError::ProviderRejected`]: permanent); 408 and
/// everything else (5xx, odd 3xx) says the provider could not answer now
/// ([`PaperError::ProviderUnavailable`]: retried, counted by the breaker).
pub fn status_error(provider: &str, status: reqwest::StatusCode) -> PaperError {
    let message = format!("{provider} returned {status}");
    if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT {
        PaperError::ProviderRejected(message)
    } else {
        PaperError::ProviderUnavailable(message)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server_429_then_200(retry_after: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", retry_after))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_huge_retry_after_sleeps_at_most_the_cap() {
        // `Retry-After: 86400` used to park the request for a day.
        let server = server_429_then_200("86400").await;
        let client = reqwest::Client::new();

        let started = Instant::now();
        let response =
            send_with_429_retry(client.get(server.uri()), "test", Duration::from_millis(300))
                .await
                .unwrap();

        assert_eq!(response.status(), 200);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "slept {:?} despite a 300 ms cap",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_unparseable_or_overflowing_retry_after_is_capped_too() {
        for header in ["99999999999999999999999", "Wed, 21 Oct 2099 07:28:00 GMT"] {
            let server = server_429_then_200(header).await;
            let client = reqwest::Client::new();
            let started = Instant::now();
            let response =
                send_with_429_retry(client.get(server.uri()), "test", Duration::from_millis(200))
                    .await
                    .unwrap();
            assert_eq!(response.status(), 200, "{header}");
            // Fallback backoff is 2-3 s; the cap clamps it to 200 ms.
            assert!(started.elapsed() < Duration::from_secs(2), "{header}");
        }
    }

    #[tokio::test]
    async fn a_retry_after_within_the_cap_is_honoured() {
        let server = server_429_then_200("1").await;
        let client = reqwest::Client::new();
        let started = Instant::now();
        send_with_429_retry(client.get(server.uri()), "test", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn a_persistent_429_reaches_the_caller_with_the_real_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7200"))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        let response =
            send_with_429_retry(client.get(server.uri()), "test", Duration::from_millis(50))
                .await
                .unwrap();
        let err = check_response(&response, "test").unwrap_err();
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7200)));
    }

    #[test]
    fn a_4xx_is_permanent_and_a_5xx_is_transient() {
        use crate::error::ErrorCategory;
        use reqwest::StatusCode;

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            let err = status_error("test", status);
            assert!(matches!(err, PaperError::ProviderRejected(_)), "{status}");
            assert!(matches!(err.category(), ErrorCategory::Permanent));
        }
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let err = status_error("test", status);
            assert!(
                matches!(err, PaperError::ProviderUnavailable(_)),
                "{status}"
            );
            assert!(matches!(err.category(), ErrorCategory::Transient));
        }
    }
}
