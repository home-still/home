//! HMAC-SHA256 compact token creation and validation.
//!
//! Token format: `base64url(payload).base64url(HMAC-SHA256(secret, payload))`
//!
//! This is intentionally simpler than JWT — fixed algorithm (HMAC-SHA256),
//! no header, single issuer/verifier. The claim set is small and fixed.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Claims embedded in a token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenClaims {
    /// Subject — device or node name (e.g., "laptop", "big", "mcp-agent")
    pub sub: String,
    /// Issued-at timestamp (Unix epoch seconds)
    pub iat: u64,
    /// Expiration timestamp (Unix epoch seconds)
    pub exp: u64,
    /// Permitted service scopes (e.g., ["scribe", "distill"])
    pub scope: Vec<String>,
}

impl TokenClaims {
    /// Check if this token grants access to a given scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.iter().any(|s| s == scope || s == "*")
    }

    /// Check if this token has expired.
    pub fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now >= self.exp
    }

    /// Seconds until expiration (0 if already expired).
    pub fn ttl_secs(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.exp.saturating_sub(now)
    }
}

/// Generate a new 256-bit random secret key for HMAC-SHA256.
pub fn generate_secret() -> Vec<u8> {
    use rand::Rng;
    let mut key = vec![0u8; 32];
    rand::rng().fill(&mut key[..]);
    key
}

/// Create a signed token string from claims and a secret key.
pub fn create_token(secret: &[u8], claims: &TokenClaims) -> Result<String, anyhow::Error> {
    let payload_json = serde_json::to_vec(claims)?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload_json);

    let mut mac =
        HmacSha256::new_from_slice(secret).map_err(|e| anyhow::anyhow!("HMAC init: {e}"))?;
    mac.update(payload_b64.as_bytes());
    let signature = mac.finalize().into_bytes();
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature);

    Ok(format!("{payload_b64}.{sig_b64}"))
}

/// Validate a token string against a secret key. Returns the claims if valid.
///
/// Checks:
/// 1. Token format (payload.signature)
/// 2. HMAC signature
/// 3. Expiration (unless `allow_expired` is true)
pub fn validate_token(
    secret: &[u8],
    token: &str,
    allow_expired: bool,
) -> Result<TokenClaims, TokenError> {
    let (payload_b64, sig_b64) = token.split_once('.').ok_or(TokenError::MalformedToken)?;

    // Verify signature
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| TokenError::InvalidSecret)?;
    mac.update(payload_b64.as_bytes());
    let expected_sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| TokenError::MalformedToken)?;
    mac.verify_slice(&expected_sig)
        .map_err(|_| TokenError::InvalidSignature)?;

    // Decode and parse claims
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| TokenError::MalformedToken)?;
    let claims: TokenClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::MalformedToken)?;

    // Check expiration
    if !allow_expired && claims.is_expired() {
        return Err(TokenError::Expired);
    }

    Ok(claims)
}

/// Validate a token, trying multiple secrets (for key rotation grace periods).
/// Returns claims on the first successful validation.
pub fn validate_token_multi(
    secrets: &[&[u8]],
    token: &str,
    allow_expired: bool,
) -> Result<TokenClaims, TokenError> {
    let mut last_err = TokenError::InvalidSignature;
    for secret in secrets {
        match validate_token(secret, token, allow_expired) {
            Ok(claims) => return Ok(claims),
            Err(TokenError::InvalidSignature) => {
                last_err = TokenError::InvalidSignature;
                continue; // Try next key
            }
            Err(e) => return Err(e), // Non-signature errors are definitive
        }
    }
    Err(last_err)
}

/// Token validation errors.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenError {
    MalformedToken,
    InvalidSecret,
    InvalidSignature,
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::MalformedToken => write!(f, "malformed token"),
            TokenError::InvalidSecret => write!(f, "invalid secret key"),
            TokenError::InvalidSignature => write!(f, "invalid signature"),
            TokenError::Expired => write!(f, "token expired"),
        }
    }
}

impl std::error::Error for TokenError {}

/// Generate a short alphanumeric enrollment code (e.g., "A7X-K9M").
pub fn generate_enrollment_code() -> String {
    use rand::Rng;
    let charset = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I to avoid confusion
    let mut rng = rand::rng();
    let code: String = (0..6)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx] as char
        })
        .collect();
    format!("{}-{}", &code[..3], &code[3..])
}

/// Current Unix epoch seconds.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> Vec<u8> {
        vec![0xAB; 32]
    }

    fn test_claims(exp_offset: i64) -> TokenClaims {
        let now = now_epoch();
        TokenClaims {
            sub: "test-device".into(),
            iat: now,
            exp: (now as i64 + exp_offset) as u64,
            scope: vec!["scribe".into(), "distill".into()],
        }
    }

    #[test]
    fn roundtrip_create_validate() {
        let secret = test_secret();
        let claims = test_claims(3600); // expires in 1 hour

        let token = create_token(&secret, &claims).unwrap();
        let validated = validate_token(&secret, &token, false).unwrap();

        assert_eq!(validated.sub, "test-device");
        assert_eq!(validated.scope, vec!["scribe", "distill"]);
    }

    #[test]
    fn wrong_secret_rejected() {
        let secret = test_secret();
        let claims = test_claims(3600);
        let token = create_token(&secret, &claims).unwrap();

        let wrong_secret = vec![0xCD; 32];
        let result = validate_token(&wrong_secret, &token, false);
        assert_eq!(result, Err(TokenError::InvalidSignature));
    }

    #[test]
    fn expired_token_rejected() {
        let secret = test_secret();
        let claims = test_claims(-60); // expired 60 seconds ago

        let token = create_token(&secret, &claims).unwrap();
        let result = validate_token(&secret, &token, false);
        assert_eq!(result, Err(TokenError::Expired));
    }

    #[test]
    fn expired_token_allowed_when_flag_set() {
        let secret = test_secret();
        let claims = test_claims(-60);

        let token = create_token(&secret, &claims).unwrap();
        let result = validate_token(&secret, &token, true);
        assert!(result.is_ok());
    }

    #[test]
    fn multi_key_validation() {
        let old_secret = vec![0xAA; 32];
        let new_secret = vec![0xBB; 32];
        let claims = test_claims(3600);

        let token = create_token(&old_secret, &claims).unwrap();
        let result = validate_token_multi(&[&new_secret, &old_secret], &token, false);
        assert!(result.is_ok());
    }

    #[test]
    fn scope_check() {
        let claims = test_claims(3600);
        assert!(claims.has_scope("scribe"));
        assert!(claims.has_scope("distill"));
        assert!(!claims.has_scope("admin"));
    }

    #[test]
    fn wildcard_scope() {
        let mut claims = test_claims(3600);
        claims.scope = vec!["*".into()];
        assert!(claims.has_scope("scribe"));
        assert!(claims.has_scope("anything"));
    }

    #[test]
    fn malformed_token_rejected() {
        let secret = test_secret();
        assert_eq!(
            validate_token(&secret, "not-a-token", false),
            Err(TokenError::MalformedToken)
        );
        assert!(validate_token(&secret, "aaa.bbb", false).is_err());
    }

    #[test]
    fn enrollment_code_format() {
        let code = generate_enrollment_code();
        assert_eq!(code.len(), 7); // "ABC-DEF"
        assert_eq!(&code[3..4], "-");
    }

    #[test]
    fn secret_generation() {
        let s1 = generate_secret();
        let s2 = generate_secret();
        assert_eq!(s1.len(), 32);
        assert_ne!(s1, s2); // should be random
    }
}
