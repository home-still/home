use crate::error::PaperError;

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
