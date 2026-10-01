//! The one place bearer tokens are read, verified and minted.
//!
//! Every authenticated endpoint (proxy, registry, token refresh) goes through
//! [`authenticate`]: signature against the current and (during a rotation grace
//! period) previous secret, token type, expiry, and revocation. There is no
//! source-address trust branch anywhere in the gateway.

use axum::http::{header, HeaderMap};
use hs_common::auth::token::{self, TokenClaims, TokenError, TokenType};

use crate::config::GatewayConfig;
use crate::state::GatewayState;

/// The services the gateway routes to, and the only scopes it will grant.
pub const SERVICES: [&str; 3] = ["scribe", "distill", "mcp"];

/// A refusal small enough to return by value: status plus a fixed message.
/// Convert with `.into_response()` at the handler boundary.
pub type Rejection = (axum::http::StatusCode, &'static str);

/// Why a bearer token was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization: Bearer` header.
    Missing,
    /// Malformed, forged, or signed with a retired key.
    Invalid,
    /// Correctly signed but past its expiry.
    Expired,
    /// Correctly signed but the wrong class (access vs refresh) for this use.
    WrongType,
    /// The subject has been revoked since this token was issued.
    Revoked,
}

/// The signing secret plus, during rotation, the one it replaced.
pub struct SigningKeys {
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
}

impl SigningKeys {
    pub fn new(current: Vec<u8>, previous: Option<Vec<u8>>) -> Self {
        Self { current, previous }
    }

    /// Load (creating the current secret on first run) per the config.
    /// A configured `previous_secret_path` that cannot be read is an error.
    pub fn load(config: &GatewayConfig) -> anyhow::Result<Self> {
        let current = token::load_or_create_secret(&config.secret_path)?;
        let previous = config
            .previous_secret_path
            .as_deref()
            .map(token::load_secret)
            .transpose()?;
        Ok(Self::new(current, previous))
    }

    /// Keys a token may be verified against, current first.
    fn verification_set(&self) -> Vec<&[u8]> {
        let mut keys: Vec<&[u8]> = vec![&self.current];
        if let Some(previous) = &self.previous {
            keys.push(previous);
        }
        keys
    }

    fn sign(&self, claims: &TokenClaims) -> anyhow::Result<String> {
        token::create_token(&self.current, claims)
    }
}

/// The token in an `Authorization: Bearer <token>` header.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Authenticate the request's bearer token as a token of class `expected`.
pub fn authenticate(
    state: &GatewayState,
    headers: &HeaderMap,
    expected: TokenType,
) -> Result<TokenClaims, AuthError> {
    let presented = bearer(headers).ok_or(AuthError::Missing)?;
    authenticate_token(state, presented, expected)
}

/// Authenticate an already-extracted token string.
pub fn authenticate_token(
    state: &GatewayState,
    presented: &str,
    expected: TokenType,
) -> Result<TokenClaims, AuthError> {
    let claims = token::validate_token(&state.keys.verification_set(), presented, expected, false)
        .map_err(|e| match e {
            TokenError::Expired => AuthError::Expired,
            TokenError::WrongType => AuthError::WrongType,
            _ => AuthError::Invalid,
        })?;
    if state.revocations.is_revoked(&claims) {
        return Err(AuthError::Revoked);
    }
    Ok(claims)
}

/// Mint a token of class `typ` for `sub` with exactly `scope`.
pub fn issue_token(
    state: &GatewayState,
    sub: &str,
    scope: &[String],
    typ: TokenType,
) -> anyhow::Result<String> {
    let now = token::now_epoch();
    let ttl = match typ {
        TokenType::Access => state.config.token_ttl_secs,
        TokenType::Refresh => state.config.refresh_ttl_secs,
    };
    state.keys.sign(&TokenClaims {
        sub: sub.to_string(),
        iat: now,
        exp: now + ttl,
        scope: scope.to_vec(),
        typ,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{bearer_headers, test_state};

    #[tokio::test]
    async fn access_tokens_authenticate_as_access_only() {
        let state = test_state(&[]).await;
        let scope = vec!["scribe".to_string()];
        let access = issue_token(&state, "laptop", &scope, TokenType::Access).unwrap();
        let refresh = issue_token(&state, "laptop", &scope, TokenType::Refresh).unwrap();

        let claims = authenticate(&state, &bearer_headers(&access), TokenType::Access).unwrap();
        assert_eq!(claims.sub, "laptop");
        assert_eq!(claims.scope, scope);
        assert_eq!(
            authenticate(&state, &bearer_headers(&refresh), TokenType::Access),
            Err(AuthError::WrongType)
        );
        assert_eq!(
            authenticate(&state, &bearer_headers(&access), TokenType::Refresh),
            Err(AuthError::WrongType)
        );
    }

    #[tokio::test]
    async fn missing_and_garbage_credentials_are_refused() {
        let state = test_state(&[]).await;
        assert_eq!(
            authenticate(&state, &HeaderMap::new(), TokenType::Access),
            Err(AuthError::Missing)
        );
        assert_eq!(
            authenticate(&state, &bearer_headers("garbage"), TokenType::Access),
            Err(AuthError::Invalid)
        );
    }

    #[tokio::test]
    async fn revoked_subjects_are_refused_but_a_new_enrollment_works() {
        let state = test_state(&[]).await;
        let scope = vec!["scribe".to_string()];
        let old = issue_token(&state, "laptop", &scope, TokenType::Access).unwrap();
        let at = state.revocations.revoke("laptop").unwrap();
        assert_eq!(
            authenticate(&state, &bearer_headers(&old), TokenType::Access),
            Err(AuthError::Revoked)
        );

        // A credential minted after the revocation instant is a fresh one.
        let fresh = state
            .keys
            .sign(&TokenClaims {
                sub: "laptop".into(),
                iat: at + 1,
                exp: at + 3600,
                scope,
                typ: TokenType::Access,
            })
            .unwrap();
        assert!(authenticate(&state, &bearer_headers(&fresh), TokenType::Access).is_ok());
    }

    #[test]
    fn previous_key_verifies_but_never_signs() {
        let old = vec![0x11; 32];
        let new = vec![0x22; 32];
        let keys = SigningKeys::new(new.clone(), Some(old.clone()));
        let claims = TokenClaims {
            sub: "laptop".into(),
            iat: token::now_epoch(),
            exp: token::now_epoch() + 60,
            scope: vec!["mcp".into()],
            typ: TokenType::Access,
        };

        let signed_with_old = token::create_token(&old, &claims).unwrap();
        assert!(token::validate_token(
            &keys.verification_set(),
            &signed_with_old,
            TokenType::Access,
            false
        )
        .is_ok());

        let minted = keys.sign(&claims).unwrap();
        assert!(token::validate_token(&[&new], &minted, TokenType::Access, false).is_ok());
        assert_eq!(
            token::validate_token(&[&old], &minted, TokenType::Access, false),
            Err(TokenError::InvalidSignature)
        );

        // Once the grace period ends (previous key dropped), old tokens die.
        let retired = SigningKeys::new(new, None);
        assert_eq!(
            token::validate_token(
                &retired.verification_set(),
                &signed_with_old,
                TokenType::Access,
                false
            ),
            Err(TokenError::InvalidSignature)
        );
    }

    #[test]
    fn a_configured_previous_secret_must_exist_and_be_valid() {
        let dir = std::env::temp_dir().join(format!("hs-gw-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = dir.join("cloud-secret.key.prev");
        let yaml = format!(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:0\n    secret_path: {}/cloud-secret.key\n    previous_secret_path: {}\n    routes:\n      mcp: http://127.0.0.1:9\n",
            dir.display(),
            prev.display()
        );
        let config =
            GatewayConfig::from_yaml(&yaml, std::path::Path::new("test-config.yaml")).unwrap();

        // Missing and too-short previous secrets are startup errors.
        assert!(SigningKeys::load(&config).is_err());
        std::fs::write(&prev, b"short").unwrap();
        assert!(SigningKeys::load(&config).is_err());

        std::fs::write(&prev, [0x33u8; 32]).unwrap();
        let keys = SigningKeys::load(&config).unwrap();
        assert_eq!(keys.previous.as_deref(), Some(&[0x33u8; 32][..]));
        // Loading again does not regenerate the current secret.
        let again = SigningKeys::load(&config).unwrap();
        assert_eq!(keys.current, again.current);
        std::fs::remove_dir_all(&dir).ok();
    }
}
