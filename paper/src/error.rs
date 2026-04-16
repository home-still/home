use thiserror::Error;

#[derive(Error, Debug)]
pub enum PaperError {
    #[error("Invalid input: {0}. See: hs paper search --help")]
    InvalidInput(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Provider unavailable: {0}. Try a different provider with --provider")]
    ProviderUnavailable(String),

    #[error("Rate limited by {provider}. Retry after {retry_after:?}. Set api_key under paper.providers.{provider} in ~/.home-still/config.yaml for higher quota.")]
    RateLimited {
        provider: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("Circuit breaker open for {0}. Provider has failed repeatedly; try again later")]
    CircuitBreakerOpen(String),

    #[error("Not found: {0}. Check the identifier or try: hs paper search")]
    NotFound(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("No download URL for paper: {0}. Try --provider to search a different source")]
    NoDownloadUrl(String),
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorCategory {
    Permanent,
    Transient,
    RateLimited,
    CircuitBreaker,
}

impl PaperError {
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidInput(_) => ErrorCategory::Permanent,
            Self::NotFound(_) => ErrorCategory::Permanent,
            Self::ParseError(_) => ErrorCategory::Permanent,
            Self::NoDownloadUrl(_) => ErrorCategory::Permanent,
            Self::Http(e) if e.is_timeout() => ErrorCategory::Transient,
            Self::Http(e) => match e.status().map(|s| s.as_u16()) {
                Some(429) => ErrorCategory::RateLimited,
                Some(500..=599) => ErrorCategory::Transient,
                Some(_) => ErrorCategory::Permanent,
                None => ErrorCategory::Transient, // connection errors
            },
            Self::Io(e) => match e.kind() {
                std::io::ErrorKind::NotFound => ErrorCategory::Permanent,
                std::io::ErrorKind::PermissionDenied => ErrorCategory::Permanent,
                _ => ErrorCategory::Transient,
            },
            Self::ProviderUnavailable(_) => ErrorCategory::Transient,
            Self::RateLimited { .. } => ErrorCategory::RateLimited,
            Self::CircuitBreakerOpen(_) => ErrorCategory::CircuitBreaker,
        }
    }

    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            PaperError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_suggests_search() {
        let err = PaperError::NotFound("10.1234/test".into());
        assert!(err.to_string().contains("hs paper search"));
    }

    #[test]
    fn invalid_input_suggests_help() {
        let err = PaperError::InvalidInput("bad query".into());
        assert!(err.to_string().contains("--help"));
    }

    #[test]
    fn provider_unavailable_suggests_flag() {
        let err = PaperError::ProviderUnavailable("arxiv".into());
        assert!(err.to_string().contains("--provider"));
    }

    #[test]
    fn rate_limited_suggests_api_key() {
        let err = PaperError::RateLimited {
            provider: "semantic_scholar".into(),
            retry_after: Some(std::time::Duration::from_secs(5)),
        };
        let s = err.to_string();
        assert!(s.contains("api_key"));
        assert!(s.contains("paper.providers.semantic_scholar"));
    }

    #[test]
    fn circuit_breaker_suggests_retry() {
        let err = PaperError::CircuitBreakerOpen("arxiv".into());
        assert!(err.to_string().contains("try again later"));
    }

    #[test]
    fn no_download_url_suggests_provider() {
        let err = PaperError::NoDownloadUrl("Some Paper Title".into());
        assert!(err.to_string().contains("--provider"));
    }
}
