//! The shared bearer secret between home-still clients and LAN backends.
//!
//! The gateway authenticates callers, then forwards to `scribe`, `distill`
//! and `mcp` servers that listen on the network. Those servers used to trust
//! whoever could reach their port, which made the gateway's authentication
//! bypassable by connecting to the backend directly (RA-24). One shared
//! secret closes that: every backend that exposes anything other than a
//! health probe requires `Authorization: Bearer <HS_BACKEND_TOKEN>`, and every
//! client that talks to a backend sends it.
//!
//! # Configuration
//!
//! * Env var [`ENV_VAR`] (`HS_BACKEND_TOKEN`), at least [`MIN_LEN`] (32) bytes
//!   of visible ASCII. Put it in `~/.home-still/secrets.env` (loaded by
//!   `hs_common::secrets::load_default_secrets`, which never overrides an
//!   already-set variable) — e.g. `openssl rand -hex 32`. The same value must
//!   be present on the gateway, on every backend and on every client host.
//! * A value that is set but unusable (too short, whitespace, control bytes)
//!   is an error naming the variable. The value itself is never printed,
//!   logged or included in `Debug` output.
//!
//! # Server side (backend)
//!
//! ```ignore
//! let token = BackendToken::from_env()?;          // refuse to start without it
//! // in the request path, before anything else:
//! if let Err(e) = token.check_authorization(request.headers()) {
//!     return unauthorized(e);                      // HTTP 401
//! }
//! ```
//!
//! `hs-common` carries no web framework, so each server wraps
//! [`BackendToken::check_authorization`] in its own middleware (axum:
//! `middleware::from_fn_with_state`). The check is the only decision; the
//! middleware only turns [`BackendAuthError`] into a 401.
//!
//! # Client side
//!
//! [`super::client::AuthedHttp::plain`] — the client used for LAN backends —
//! attaches the header automatically when the variable is set. Cloud clients
//! (`AuthedHttp::with_auth`) are unchanged: they carry the gateway token.
//!
//! # Comparison
//!
//! Both sides of the comparison are hashed with SHA-256 first and the digests
//! compared in constant time, so neither the contents nor the length of the
//! secret leaks through timing.

use std::fmt;
use std::sync::Arc;

use http::header::AUTHORIZATION;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Name of the environment variable that carries the secret.
pub const ENV_VAR: &str = "HS_BACKEND_TOKEN";

/// Shortest accepted secret, in bytes.
pub const MIN_LEN: usize = 32;

/// Why a secret was rejected. Never contains the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTokenError {
    TooShort {
        len: usize,
    },
    /// Anything but visible ASCII (`0x21..=0x7e`): spaces, control bytes and
    /// non-ASCII cannot be sent in a header and are almost always a
    /// copy-paste accident.
    InvalidCharacter,
}

impl fmt::Display for BackendTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(
                f,
                "is {len} bytes long; at least {MIN_LEN} are required (generate one with `openssl rand -hex 32`)"
            ),
            Self::InvalidCharacter => f.write_str(
                "contains whitespace, control or non-ASCII characters; use visible ASCII only",
            ),
        }
    }
}

impl std::error::Error for BackendTokenError {}

/// Why a request was refused. Safe to log and to return to the caller: it
/// says what was wrong with the header, never what the secret is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAuthError {
    /// No `Authorization` header.
    Missing,
    /// A header that is not exactly `Bearer <token>` (other scheme, extra
    /// whitespace, empty token, several `Authorization` headers, non-UTF-8).
    Malformed,
    /// A well-formed bearer token that is not the secret.
    Mismatch,
}

impl fmt::Display for BackendAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "missing bearer token",
            Self::Malformed => "malformed Authorization header (expected `Bearer <token>`)",
            Self::Mismatch => "invalid bearer token",
        })
    }
}

impl std::error::Error for BackendAuthError {}

/// The shared secret. Cheap to clone; `Debug` is redacted.
#[derive(Clone)]
pub struct BackendToken {
    inner: Arc<Inner>,
}

struct Inner {
    secret: String,
    digest: [u8; 32],
}

impl BackendToken {
    /// Validate and wrap a secret.
    pub fn new(secret: &str) -> Result<Self, BackendTokenError> {
        if secret.len() < MIN_LEN {
            return Err(BackendTokenError::TooShort { len: secret.len() });
        }
        if !secret.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(BackendTokenError::InvalidCharacter);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                secret: secret.to_string(),
                digest: Sha256::digest(secret.as_bytes()).into(),
            }),
        })
    }

    /// The secret from [`ENV_VAR`]. For servers: unset or unusable is an
    /// error that names the variable, so the server refuses to start.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name))
    }

    /// The secret from [`ENV_VAR`] when it is set; `Ok(None)` when it is not.
    /// For clients: a backend that requires the token will answer 401, and
    /// one that does not simply ignores it. A value that is set but unusable
    /// is still an error.
    pub fn from_env_optional() -> anyhow::Result<Option<Self>> {
        Self::optional_from_lookup(|name| std::env::var(name))
    }

    /// [`Self::from_env`] over an arbitrary lookup (tests, embedding).
    pub fn from_lookup(
        get: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Self> {
        Self::optional_from_lookup(get)?.ok_or_else(|| {
            anyhow::anyhow!(
                "{ENV_VAR} is not set; set it (at least {MIN_LEN} bytes, e.g. \
                 `openssl rand -hex 32`) in the environment or in ~/.home-still/secrets.env"
            )
        })
    }

    /// [`Self::from_env_optional`] over an arbitrary lookup.
    pub fn optional_from_lookup(
        get: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Option<Self>> {
        match get(ENV_VAR) {
            Ok(raw) => Self::new(&raw)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("{ENV_VAR} {e}")),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("{ENV_VAR} is not valid UTF-8")
            }
        }
    }

    /// Constant-time equality with a candidate secret.
    pub fn matches(&self, candidate: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        digest.ct_eq(&self.inner.digest).into()
    }

    /// Check the `Authorization` header of a request.
    ///
    /// Accepts exactly `Authorization: Bearer <token>` (scheme compared
    /// case-insensitively, as RFC 7235 requires; one space; no surrounding
    /// whitespace) and nothing else: no query parameters, cookies or other
    /// schemes.
    pub fn check_authorization(&self, headers: &HeaderMap) -> Result<(), BackendAuthError> {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let value = values.next().ok_or(BackendAuthError::Missing)?;
        if values.next().is_some() {
            return Err(BackendAuthError::Malformed);
        }
        let value = value.to_str().map_err(|_| BackendAuthError::Malformed)?;
        let (scheme, token) = value.split_once(' ').ok_or(BackendAuthError::Malformed)?;
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.bytes().any(|b| !(0x21..=0x7e).contains(&b))
        {
            return Err(BackendAuthError::Malformed);
        }
        if self.matches(token) {
            Ok(())
        } else {
            Err(BackendAuthError::Mismatch)
        }
    }

    /// The secret, for the one place that must put it on the wire (a client
    /// attaching `Authorization: Bearer`). Never log the result.
    pub fn expose_secret(&self) -> &str {
        &self.inner.secret
    }
}

impl fmt::Debug for BackendToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BackendToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn token() -> BackendToken {
        BackendToken::new(SECRET).unwrap()
    }

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn the_exact_bearer_header_is_accepted() {
        assert_eq!(
            token().check_authorization(&headers(&format!("Bearer {SECRET}"))),
            Ok(())
        );
    }

    #[test]
    fn a_request_without_the_header_is_missing() {
        assert_eq!(
            token().check_authorization(&HeaderMap::new()),
            Err(BackendAuthError::Missing)
        );
    }

    #[test]
    fn a_wrong_token_of_the_right_length_is_a_mismatch() {
        let wrong = format!("Bearer {}", "x".repeat(SECRET.len()));
        assert_eq!(
            token().check_authorization(&headers(&wrong)),
            Err(BackendAuthError::Mismatch)
        );
    }

    #[test]
    fn a_prefix_or_extension_of_the_secret_is_a_mismatch() {
        let t = token();
        for candidate in [&SECRET[..SECRET.len() - 1], &format!("{SECRET}x")] {
            assert_eq!(
                t.check_authorization(&headers(&format!("Bearer {candidate}"))),
                Err(BackendAuthError::Mismatch),
                "{candidate}"
            );
        }
    }

    #[test]
    fn only_a_well_formed_bearer_header_is_inspected() {
        let t = token();
        for (header, why) in [
            (format!("Bearer  {SECRET}"), "two spaces"),
            (format!("Bearer {SECRET} "), "trailing space"),
            (format!(" Bearer {SECRET}"), "leading space"),
            (format!("Bearer\t{SECRET}"), "tab separator"),
            ("Bearer ".to_string(), "empty token"),
            ("Bearer".to_string(), "no token"),
            (String::new(), "empty header"),
            (format!("Basic {SECRET}"), "other scheme"),
            (SECRET.to_string(), "bare token"),
            (format!("Bearer {SECRET} extra"), "trailing parameter"),
        ] {
            assert_eq!(
                t.check_authorization(&headers(&header)),
                Err(BackendAuthError::Malformed),
                "{why}: {header:?}"
            );
        }
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        for scheme in ["bearer", "BEARER", "BeArEr"] {
            assert_eq!(
                token().check_authorization(&headers(&format!("{scheme} {SECRET}"))),
                Ok(()),
                "{scheme}"
            );
        }
    }

    #[test]
    fn two_authorization_headers_are_refused_even_if_one_is_right() {
        let mut h = headers(&format!("Bearer {SECRET}"));
        h.append(AUTHORIZATION, HeaderValue::from_static("Bearer other"));
        assert_eq!(
            token().check_authorization(&h),
            Err(BackendAuthError::Malformed)
        );
    }

    #[test]
    fn non_utf8_header_bytes_are_malformed() {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff\xfe").unwrap(),
        );
        assert_eq!(
            token().check_authorization(&h),
            Err(BackendAuthError::Malformed)
        );
    }

    #[test]
    fn short_and_unprintable_secrets_are_rejected() {
        assert_eq!(
            BackendToken::new(&"a".repeat(MIN_LEN - 1)).unwrap_err(),
            BackendTokenError::TooShort { len: MIN_LEN - 1 }
        );
        assert!(BackendToken::new(&"a".repeat(MIN_LEN)).is_ok());
        for bad in [
            format!("{} {}", "a".repeat(20), "b".repeat(20)),
            format!("{}\n", "a".repeat(MIN_LEN)),
            format!("{}é", "a".repeat(MIN_LEN)),
        ] {
            assert_eq!(
                BackendToken::new(&bad).unwrap_err(),
                BackendTokenError::InvalidCharacter,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn matches_is_exact_for_every_length() {
        let t = token();
        assert!(t.matches(SECRET));
        assert!(!t.matches(""));
        assert!(!t.matches(&SECRET.to_uppercase()));
        assert!(!t.matches(&SECRET[1..]));
    }

    #[test]
    fn env_loading_names_the_variable_and_never_prints_the_value() {
        let unset = BackendToken::from_lookup(|_| Err(std::env::VarError::NotPresent))
            .unwrap_err()
            .to_string();
        assert!(unset.contains(ENV_VAR), "{unset}");

        let short = BackendToken::from_lookup(|_| Ok("hunter2-hunter2".into()))
            .unwrap_err()
            .to_string();
        assert!(short.contains(ENV_VAR), "{short}");
        assert!(!short.contains("hunter2"), "the value leaked: {short}");

        let ok = BackendToken::from_lookup(|_| Ok(SECRET.into())).unwrap();
        assert!(ok.matches(SECRET));
    }

    #[test]
    fn optional_loading_distinguishes_unset_from_unusable() {
        assert!(
            BackendToken::optional_from_lookup(|_| Err(std::env::VarError::NotPresent))
                .unwrap()
                .is_none()
        );
        assert!(BackendToken::optional_from_lookup(|_| Ok("short".into())).is_err());
        assert!(BackendToken::optional_from_lookup(|_| Ok(SECRET.into()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn debug_output_does_not_contain_the_secret() {
        let shown = format!("{:?}", token());
        assert!(!shown.contains(SECRET), "{shown}");
    }
}
