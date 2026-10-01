//! HMAC-SHA256 compact token creation and validation.
//!
//! Token format: `base64url(payload).base64url(HMAC-SHA256(secret, payload))`
//!
//! This is intentionally simpler than JWT — fixed algorithm (HMAC-SHA256),
//! no header, single issuer/verifier. The claim set is small and fixed.
//!
//! Every token carries a `typ` claim ([`TokenType`]). Access tokens
//! authenticate requests; refresh tokens are only good for minting access
//! tokens. [`validate_token`] takes the expected type, so a token of the
//! wrong class — or an older token with no `typ` at all — can never be
//! accepted by accident.
//!
//! This module also owns the gateway's on-disk credentials: the signing
//! secret and the admin key. Both are created with mode 0600 at creation time
//! (no world-readable window) and a malformed existing file is an error, never
//! silently regenerated.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Token class, carried in the `typ` claim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenType {
    /// Short-lived; authenticates proxy and registry requests.
    Access,
    /// Long-lived; only accepted by the refresh endpoints.
    Refresh,
}

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
    /// Token class (access or refresh)
    pub typ: TokenType,
}

impl TokenClaims {
    /// Check if this token grants access to a given scope. Exact match only:
    /// there is no wildcard scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scope.iter().any(|s| s == scope)
    }

    /// Check if this token has expired.
    pub fn is_expired(&self) -> bool {
        now_epoch() >= self.exp
    }

    /// Seconds until expiration (0 if already expired).
    pub fn ttl_secs(&self) -> u64 {
        self.exp.saturating_sub(now_epoch())
    }
}

/// Generate a new 256-bit random secret key for HMAC-SHA256.
pub fn generate_secret() -> Vec<u8> {
    use rand::Rng;
    let mut key = vec![0u8; MIN_SECRET_LEN];
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

/// Validate a token string. Returns the claims if valid.
///
/// `secrets` is the verification key set: the current signing secret first,
/// then (during a rotation grace period) the previous one. The token is valid
/// if its signature matches any of them.
///
/// Checks:
/// 1. Token format (payload.signature)
/// 2. HMAC signature against `secrets`
/// 3. Token type equals `expected` (a token with no `typ` is malformed)
/// 4. Expiration (unless `allow_expired` is true)
pub fn validate_token(
    secrets: &[&[u8]],
    token: &str,
    expected: TokenType,
    allow_expired: bool,
) -> Result<TokenClaims, TokenError> {
    let (payload_b64, sig_b64) = token.split_once('.').ok_or(TokenError::MalformedToken)?;
    let signature = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| TokenError::MalformedToken)?;

    if secrets.is_empty() {
        return Err(TokenError::InvalidSecret);
    }
    let mut signed = false;
    for secret in secrets {
        let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| TokenError::InvalidSecret)?;
        mac.update(payload_b64.as_bytes());
        if mac.verify_slice(&signature).is_ok() {
            signed = true;
            break;
        }
    }
    if !signed {
        return Err(TokenError::InvalidSignature);
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| TokenError::MalformedToken)?;
    let claims: TokenClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::MalformedToken)?;

    if claims.typ != expected {
        return Err(TokenError::WrongType);
    }
    if !allow_expired && claims.is_expired() {
        return Err(TokenError::Expired);
    }

    Ok(claims)
}

/// Decode a token's claims WITHOUT verifying the signature. For display on the
/// client that holds the token (e.g. showing when it expires); never use it to
/// make an authorization decision.
pub fn decode_claims_unverified(token: &str) -> Option<TokenClaims> {
    let (payload_b64, _) = token.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// Token validation errors.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenError {
    MalformedToken,
    InvalidSecret,
    InvalidSignature,
    /// Signature is good but the token is the wrong class for this use
    /// (e.g. a refresh token presented to the proxy).
    WrongType,
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::MalformedToken => write!(f, "malformed token"),
            TokenError::InvalidSecret => write!(f, "invalid secret key"),
            TokenError::InvalidSignature => write!(f, "invalid signature"),
            TokenError::WrongType => write!(f, "wrong token type"),
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

// ── Gateway credential files ───────────────────────────────────

/// Minimum accepted length of the signing secret (bytes) and of the admin key
/// (characters).
pub const MIN_SECRET_LEN: usize = 32;

/// Default location of the gateway signing secret.
pub fn default_secret_path() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(crate::HIDDEN_DIR).join("cloud-secret.key"),
        // No home dir means there is no config file to read either; keep the
        // path relative rather than inventing an absolute one.
        None => PathBuf::from(crate::HIDDEN_DIR).join("cloud-secret.key"),
    }
}

/// Location of the admin key: `cloud-admin.key` beside the signing secret.
///
/// The admin key authenticates `hs cloud invite` / `hs cloud revoke` against
/// the gateway's admin endpoints. It is deliberately a different secret from
/// the signing secret, so possessing a (stolen) signed token — or the ability
/// to sign one — grants no admin access, and vice versa.
pub fn admin_key_path_for(secret_path: &Path) -> PathBuf {
    secret_path.with_file_name("cloud-admin.key")
}

/// Generate a fresh admin key: 256 random bits, base64url (43 characters).
pub fn generate_admin_key() -> String {
    URL_SAFE_NO_PAD.encode(generate_secret())
}

/// Write `contents` to a brand-new file that is mode 0600 from the moment it
/// exists. Fails with `AlreadyExists` rather than touching an existing file.
fn create_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Read the signing secret at `path`. A file shorter than [`MIN_SECRET_LEN`]
/// bytes is an error: regenerating it would silently invalidate every token.
pub fn load_secret(path: &Path) -> anyhow::Result<Vec<u8>> {
    let data = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("reading secret {}: {e}", path.display()))?;
    if data.len() < MIN_SECRET_LEN {
        anyhow::bail!(
            "secret {} is {} bytes, shorter than the required {MIN_SECRET_LEN}; \
             refusing to regenerate it (that would invalidate every issued token). \
             Restore the file, or delete it deliberately to start a new secret.",
            path.display(),
            data.len()
        );
    }
    Ok(data)
}

/// Load the signing secret at `path`, creating it (mode 0600, atomically) if
/// the file does not exist.
pub fn load_or_create_secret(path: &Path) -> anyhow::Result<Vec<u8>> {
    let secret = generate_secret();
    match create_private_file(path, &secret) {
        Ok(()) => Ok(secret),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_secret(path),
        Err(e) => Err(anyhow::anyhow!("creating secret {}: {e}", path.display())),
    }
}

/// Read the admin key at `path` (surrounding whitespace trimmed). A key
/// shorter than [`MIN_SECRET_LEN`] characters is an error.
pub fn load_admin_key(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading admin key {}: {e}", path.display()))?;
    let key = data.trim();
    if key.len() < MIN_SECRET_LEN {
        anyhow::bail!(
            "admin key {} is {} characters, shorter than the required {MIN_SECRET_LEN}; \
             delete the file deliberately to generate a new one",
            path.display(),
            key.len()
        );
    }
    Ok(key.to_string())
}

/// Load the admin key at `path`, creating it (mode 0600, atomically) if the
/// file does not exist.
pub fn load_or_create_admin_key(path: &Path) -> anyhow::Result<String> {
    let key = generate_admin_key();
    match create_private_file(path, key.as_bytes()) {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_admin_key(path),
        Err(e) => Err(anyhow::anyhow!(
            "creating admin key {}: {e}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secret() -> Vec<u8> {
        vec![0xAB; 32]
    }

    fn test_claims(exp_offset: i64) -> TokenClaims {
        typed_claims(TokenType::Access, exp_offset)
    }

    fn typed_claims(typ: TokenType, exp_offset: i64) -> TokenClaims {
        let now = now_epoch();
        TokenClaims {
            sub: "test-device".into(),
            iat: now,
            exp: (now as i64 + exp_offset) as u64,
            scope: vec!["scribe".into(), "distill".into()],
            typ,
        }
    }

    fn validate(secret: &[u8], token: &str) -> Result<TokenClaims, TokenError> {
        validate_token(&[secret], token, TokenType::Access, false)
    }

    #[test]
    fn roundtrip_create_validate() {
        let secret = test_secret();
        let claims = test_claims(3600); // expires in 1 hour

        let token = create_token(&secret, &claims).unwrap();
        let validated = validate(&secret, &token).unwrap();

        assert_eq!(validated.sub, "test-device");
        assert_eq!(validated.scope, vec!["scribe", "distill"]);
        assert_eq!(validated.typ, TokenType::Access);
    }

    #[test]
    fn wrong_secret_rejected() {
        let secret = test_secret();
        let claims = test_claims(3600);
        let token = create_token(&secret, &claims).unwrap();

        let wrong_secret = vec![0xCD; 32];
        let result = validate(&wrong_secret, &token);
        assert_eq!(result, Err(TokenError::InvalidSignature));
    }

    #[test]
    fn expired_token_rejected() {
        let secret = test_secret();
        let claims = test_claims(-60); // expired 60 seconds ago

        let token = create_token(&secret, &claims).unwrap();
        let result = validate(&secret, &token);
        assert_eq!(result, Err(TokenError::Expired));
    }

    #[test]
    fn expired_token_allowed_when_flag_set() {
        let secret = test_secret();
        let claims = test_claims(-60);

        let token = create_token(&secret, &claims).unwrap();
        let result = validate_token(&[&secret], &token, TokenType::Access, true);
        assert!(result.is_ok());
    }

    #[test]
    fn multi_key_validation() {
        let old_secret = vec![0xAA; 32];
        let new_secret = vec![0xBB; 32];
        let claims = test_claims(3600);

        let token = create_token(&old_secret, &claims).unwrap();
        let result = validate_token(
            &[&new_secret, &old_secret],
            &token,
            TokenType::Access,
            false,
        );
        assert!(result.is_ok());

        let unrelated = vec![0xCC; 32];
        assert_eq!(
            validate_token(&[&new_secret, &unrelated], &token, TokenType::Access, false),
            Err(TokenError::InvalidSignature)
        );
    }

    #[test]
    fn empty_key_set_rejects_everything() {
        let token = create_token(&test_secret(), &test_claims(3600)).unwrap();
        assert_eq!(
            validate_token(&[], &token, TokenType::Access, false),
            Err(TokenError::InvalidSecret)
        );
    }

    #[test]
    fn token_of_the_wrong_class_is_rejected_both_ways() {
        let secret = test_secret();
        let access = create_token(&secret, &typed_claims(TokenType::Access, 3600)).unwrap();
        let refresh = create_token(&secret, &typed_claims(TokenType::Refresh, 3600)).unwrap();

        assert!(validate_token(&[&secret], &access, TokenType::Access, false).is_ok());
        assert!(validate_token(&[&secret], &refresh, TokenType::Refresh, false).is_ok());
        assert_eq!(
            validate_token(&[&secret], &refresh, TokenType::Access, false),
            Err(TokenError::WrongType)
        );
        assert_eq!(
            validate_token(&[&secret], &access, TokenType::Refresh, false),
            Err(TokenError::WrongType)
        );
    }

    #[test]
    fn token_without_typ_claim_is_never_accepted() {
        // A correctly signed payload from before the `typ` claim existed.
        let secret = test_secret();
        let now = now_epoch();
        let payload = serde_json::json!({
            "sub": "old-device",
            "iat": now,
            "exp": now + 3600,
            "scope": ["scribe"],
        });
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
        mac.update(payload_b64.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let token = format!("{payload_b64}.{sig}");

        for typ in [TokenType::Access, TokenType::Refresh] {
            assert_eq!(
                validate_token(&[&secret], &token, typ, false),
                Err(TokenError::MalformedToken)
            );
        }
    }

    #[test]
    fn scope_check() {
        let claims = test_claims(3600);
        assert!(claims.has_scope("scribe"));
        assert!(claims.has_scope("distill"));
        assert!(!claims.has_scope("admin"));
    }

    #[test]
    fn wildcard_scope_grants_nothing() {
        let mut claims = test_claims(3600);
        claims.scope = vec!["*".into()];
        assert!(!claims.has_scope("scribe"));
        assert!(!claims.has_scope("anything"));
    }

    #[test]
    fn malformed_token_rejected() {
        let secret = test_secret();
        assert_eq!(
            validate(&secret, "not-a-token"),
            Err(TokenError::MalformedToken)
        );
        assert!(validate(&secret, "aaa.bbb").is_err());
    }

    #[test]
    fn decode_unverified_reads_claims_without_a_key() {
        let claims = test_claims(3600);
        let token = create_token(&test_secret(), &claims).unwrap();
        assert_eq!(decode_claims_unverified(&token), Some(claims));
        assert_eq!(decode_claims_unverified("garbage"), None);
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hs-token-test-{name}-{}-{}",
            std::process::id(),
            now_epoch()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn secret_file_is_created_private_and_reloaded_unchanged() {
        let dir = temp_dir("secret-create");
        let path = dir.join("nested").join("cloud-secret.key");

        let first = load_or_create_secret(&path).unwrap();
        assert_eq!(first.len(), MIN_SECRET_LEN);
        #[cfg(unix)]
        assert_eq!(mode_of(&path), 0o600);

        let second = load_or_create_secret(&path).unwrap();
        assert_eq!(first, second, "an existing secret must never be replaced");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn short_secret_file_is_an_error_and_is_left_alone() {
        let dir = temp_dir("secret-short");
        let path = dir.join("cloud-secret.key");
        std::fs::write(&path, b"too-short").unwrap();

        let err = load_or_create_secret(&path).unwrap_err();
        assert!(format!("{err:#}").contains("shorter"), "{err:#}");
        assert_eq!(std::fs::read(&path).unwrap(), b"too-short");
        assert!(load_secret(&path).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn admin_key_is_created_private_beside_the_secret_and_distinct_from_it() {
        let dir = temp_dir("admin-key");
        let secret_path = dir.join("cloud-secret.key");
        let admin_path = admin_key_path_for(&secret_path);
        assert_eq!(admin_path.parent(), secret_path.parent());
        assert_ne!(admin_path, secret_path);

        let secret = load_or_create_secret(&secret_path).unwrap();
        let key = load_or_create_admin_key(&admin_path).unwrap();
        #[cfg(unix)]
        assert_eq!(mode_of(&admin_path), 0o600);
        assert!(key.len() >= MIN_SECRET_LEN);
        assert_ne!(key.as_bytes(), &secret[..]);

        assert_eq!(load_or_create_admin_key(&admin_path).unwrap(), key);
        assert_eq!(load_admin_key(&admin_path).unwrap(), key);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn short_admin_key_is_an_error() {
        let dir = temp_dir("admin-short");
        let path = dir.join("cloud-admin.key");
        std::fs::write(&path, "tiny\n").unwrap();
        assert!(load_or_create_admin_key(&path).is_err());
        assert!(load_admin_key(&path).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
