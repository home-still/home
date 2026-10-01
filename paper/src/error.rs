use thiserror::Error;

use crate::models::ProviderFailure;

#[derive(Error, Debug)]
pub enum PaperError {
    #[error("Invalid input: {0}. See: hs paper search --help")]
    InvalidInput(String),

    /// Built through `From<reqwest::Error>`, which drops the URL's query
    /// string: reqwest prints the full request URL, and ours carry API keys
    /// (`api_key=`) and contact emails (`email=`, `mailto=`).
    #[error("HTTP error: {0}")]
    Http(#[source] reqwest::Error),

    #[error("Provider unavailable: {0}. Try a different provider with --provider")]
    ProviderUnavailable(String),

    #[error("Rate limited by {provider} (retry-after: {retry_after:?}). Wait ~30 seconds and retry this exact call, or try a different provider (arxiv, openalex, europmc, crossref, core).")]
    RateLimited {
        provider: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("Circuit breaker open for {0}. Provider has failed repeatedly; try again later")]
    CircuitBreakerOpen(String),

    #[error("Not found: {0}. Check the identifier or try: hs paper search")]
    NotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("No download URL for paper: {0}. Try --provider to search a different source")]
    NoDownloadUrl(String),

    /// A fan-out over several providers produced no answer: either every
    /// provider failed (`succeeded == 0`), or none had the paper and some
    /// failed, so its absence is unconfirmed. Lists each failure.
    #[error("{}", format_provider_failures(*.succeeded, .failures))]
    ProvidersFailed {
        succeeded: usize,
        failures: Vec<ProviderFailure>,
    },

    /// The storage backend failed (head/put/verify/invalid key). Local to
    /// this host: no other source can fix it, so a download aborts on it.
    #[error("Storage error: {0}")]
    Storage(String),

    /// A URL supplied by a resolver or a redirect was refused before any
    /// request was sent (or at connect time, for a hostname that resolved to
    /// a non-public address). See `providers::url_guard`.
    #[error("Refusing to fetch {url}: {reason}")]
    UnsafeUrl { url: String, reason: String },

    /// The response body exceeded `download.max_download_bytes`.
    #[error(
        "Response from {url} exceeds the {limit}-byte download limit (download.max_download_bytes)"
    )]
    TooLarge { url: String, limit: u64 },

    /// The response body is not a PDF (`%PDF-` magic missing) — a landing
    /// page, a paywall, an image. Nothing is stored.
    #[error("Response from {url} is not a PDF: {detail}")]
    NotPdf { url: String, detail: String },

    /// Every source of the resolver chain was tried and none produced a PDF.
    /// Carries one outcome per source so the caller can tell "nobody has an
    /// open copy" from "three services were down".
    #[error("No PDF could be downloaded for DOI {doi}. Per-source outcomes:{}", format_outcomes(.sources))]
    NoSourceYielded {
        doi: String,
        sources: Vec<SourceOutcome>,
    },
}

impl From<reqwest::Error> for PaperError {
    fn from(mut err: reqwest::Error) -> Self {
        if let Some(url) = err.url_mut() {
            url.set_query(None);
            url.set_fragment(None);
        }
        PaperError::Http(err)
    }
}

/// What one source of the download chain reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOutcome {
    pub source: String,
    pub kind: OutcomeKind,
    pub detail: String,
}

impl SourceOutcome {
    pub fn no_copy(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            kind: OutcomeKind::NoCopy,
            detail: detail.into(),
        }
    }

    pub fn failed(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            kind: OutcomeKind::Failed,
            detail: detail.into(),
        }
    }
}

/// Why a source produced no PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The source answered and has no usable copy (not open access, no
    /// record, the URL serves a landing page, not configured).
    NoCopy,
    /// The source could not be asked or misbehaved (network error, HTTP
    /// error status, rate limit, open circuit, refused URL, oversized body).
    Failed,
}

fn format_outcomes(sources: &[SourceOutcome]) -> String {
    let mut out = String::new();
    for s in sources {
        let kind = match s.kind {
            OutcomeKind::NoCopy => "no copy",
            OutcomeKind::Failed => "failed",
        };
        out.push_str(&format!("\n  - {}: {} ({})", s.source, kind, s.detail));
    }
    out
}

fn format_provider_failures(succeeded: usize, failures: &[ProviderFailure]) -> String {
    let mut out = if succeeded == 0 {
        format!("All {} providers failed:", failures.len())
    } else {
        format!(
            "No provider returned the paper and {} failed, so its absence is unconfirmed:",
            failures.len()
        )
    };
    for f in failures {
        out.push_str(&format!("\n  - {}: {}", f.provider, f.error));
    }
    out
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
            Self::UnsafeUrl { .. } => ErrorCategory::Permanent,
            Self::TooLarge { .. } => ErrorCategory::Permanent,
            Self::NotPdf { .. } => ErrorCategory::Permanent,
            Self::Storage(_) => ErrorCategory::Transient,
            Self::ProvidersFailed { .. } => ErrorCategory::Transient,
            Self::NoSourceYielded { sources, .. } => {
                if sources.iter().any(|s| s.kind == OutcomeKind::Failed) {
                    ErrorCategory::Transient
                } else {
                    ErrorCategory::Permanent
                }
            }
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

    /// True when the failure is on this host — storage, the filesystem, or a
    /// malformed identifier — rather than something a different source could
    /// answer differently. The download chain aborts on these immediately
    /// instead of trying the next source and reporting "not found".
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Storage(_) | Self::InvalidInput(_))
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
    fn rate_limited_directs_retry() {
        let err = PaperError::RateLimited {
            provider: "semantic_scholar".into(),
            retry_after: Some(std::time::Duration::from_secs(5)),
        };
        let s = err.to_string();
        assert!(s.contains("Wait"), "got: {s}");
        assert!(s.contains("retry"), "got: {s}");
        assert!(s.contains("different provider"), "got: {s}");
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
