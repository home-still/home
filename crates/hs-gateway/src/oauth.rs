//! OAuth 2.1 Authorization Code + PKCE flow for Claude Desktop MCP access.
//!
//! Implements the endpoints Claude Desktop needs to authenticate with the gateway:
//! - Well-known discovery endpoints (RFC 8414, RFC 9728)
//! - Authorization endpoint (enrollment code form)
//! - Token endpoint (code exchange + PKCE verification + refresh)
//! - Dynamic Client Registration (RFC 7591)
//!
//! The user authorizes by entering an administrator-issued enrollment code, and
//! the resulting tokens carry exactly the scopes that code was issued with.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hs_common::auth::token::TokenType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::auth;
use crate::enrollment::normalize_code;
use crate::state::GatewayState;
use crate::store::ExpiringStore;

// ── In-memory stores ───────────────────────────────────────────

/// How long an authorization code can be exchanged for tokens.
const AUTH_CODE_TTL: Duration = Duration::from_secs(60);
const MAX_AUTH_CODES: usize = 256;

/// How long a dynamically registered client is remembered. It only matters
/// while a user is mid-authorization (tokens do not depend on it), so this is
/// generous; the gateway forgets clients on restart anyway.
const CLIENT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_CLIENTS: usize = 1024;

const MAX_REDIRECT_URIS: usize = 10;
const MAX_REDIRECT_URI_LEN: usize = 2048;
const MAX_CLIENT_NAME_LEN: usize = 100;
const MAX_STATE_LEN: usize = 512;

/// A pending OAuth authorization code waiting to be exchanged.
pub struct PendingAuthCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    /// Scopes of the enrollment code the user presented.
    scopes: Vec<String>,
}

/// A dynamically registered OAuth client.
#[derive(Clone, Serialize)]
pub struct RegisteredClient {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
}

pub type AuthCodeStore = ExpiringStore<PendingAuthCode>;
pub type ClientStore = ExpiringStore<RegisteredClient>;

pub fn new_auth_code_store() -> AuthCodeStore {
    ExpiringStore::new(AUTH_CODE_TTL, MAX_AUTH_CODES)
}

pub fn new_client_store() -> ClientStore {
    ExpiringStore::new(CLIENT_TTL, MAX_CLIENTS)
}

// ── Well-known endpoints ───────────────────────────────────────

/// GET /.well-known/oauth-protected-resource (RFC 9728)
pub async fn handle_protected_resource_metadata(
    State(state): State<Arc<GatewayState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "resource": state.gateway_url,
        "authorization_servers": [state.gateway_url],
        "scopes_supported": ["mcp:tools"],
    }))
}

/// GET /.well-known/oauth-authorization-server (RFC 8414)
pub async fn handle_auth_server_metadata(
    State(state): State<Arc<GatewayState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "issuer": state.gateway_url,
        "authorization_endpoint": format!("{}/authorize", state.gateway_url),
        "token_endpoint": format!("{}/token", state.gateway_url),
        "registration_endpoint": format!("{}/register", state.gateway_url),
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp:tools"],
    }))
}

// ── Authorization endpoint ─────────────────────────────────────

/// Escape text for interpolation into HTML element content or a quoted
/// attribute value.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// Wrap an HTML body with headers that keep the page from being framed,
/// cached, leaked via Referer, or from running any script at all.
fn html_response(status: StatusCode, body: String) -> Response {
    let mut resp = (status, Html(body)).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp
}

fn error_page(status: StatusCode, title: &str, message: &str) -> Response {
    html_response(
        status,
        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>body {{ font-family: system-ui; max-width: 400px; margin: 80px auto; text-align: center; }}</style>
</head><body>
<h2>{title}</h2>
<p>{message}</p>
</body></html>"#,
            title = html_escape(title),
            message = html_escape(message),
        ),
    )
}

/// The authorization request parameters, as received on GET (query) and POST
/// (form). All of them are attacker-controlled until validated.
#[derive(Deserialize)]
pub struct AuthorizeParams {
    client_id: Option<String>,
    redirect_uri: Option<String>,
    response_type: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

/// An authorization request that passed validation.
struct ValidAuthorize {
    client: RegisteredClient,
    redirect_uri: String,
    state: String,
    code_challenge: String,
}

/// A PKCE S256 challenge is base64url(SHA-256(verifier)): 43 URL-safe chars.
fn is_s256_challenge(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Validate an authorization request against the registered client.
///
/// The `redirect_uri` must be one of the client's registered URIs *exactly*;
/// a request that fails any check is never redirected anywhere.
fn validate_authorize(
    state: &GatewayState,
    p: &AuthorizeParams,
) -> Result<ValidAuthorize, &'static str> {
    if p.response_type.as_deref() != Some("code") {
        return Err("Only response_type=code is supported.");
    }
    let client_id = p.client_id.as_deref().ok_or("Missing client_id.")?;
    let client = state
        .oauth_clients
        .get(client_id)
        .ok_or("Unknown client. Register it first (POST /register).")?;
    let redirect_uri = p.redirect_uri.as_deref().ok_or("Missing redirect_uri.")?;
    if !client.redirect_uris.iter().any(|u| u == redirect_uri) {
        return Err("redirect_uri is not registered for this client.");
    }
    if p.code_challenge_method.as_deref() != Some("S256") {
        return Err("PKCE is required: code_challenge_method must be S256.");
    }
    let code_challenge = p
        .code_challenge
        .as_deref()
        .filter(|c| is_s256_challenge(c))
        .ok_or("code_challenge must be a base64url SHA-256 digest.")?;
    let oauth_state = p.state.as_deref().unwrap_or_default();
    if oauth_state.len() > MAX_STATE_LEN {
        return Err("state is too long.");
    }
    Ok(ValidAuthorize {
        redirect_uri: redirect_uri.to_string(),
        state: oauth_state.to_string(),
        code_challenge: code_challenge.to_string(),
        client,
    })
}

/// GET /authorize — show enrollment code form
pub async fn handle_authorize_get(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let req = match validate_authorize(&state, &params) {
        Ok(r) => r,
        Err(why) => {
            return error_page(
                StatusCode::BAD_REQUEST,
                "Invalid authorization request",
                why,
            )
        }
    };

    let returns_to = Url::parse(&req.redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();

    html_response(
        StatusCode::OK,
        format!(
            r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Home-Still Cloud</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 400px;
         margin: 80px auto; text-align: center; background: #fafafa; color: #333; }}
  h2 {{ color: #1a1a2e; margin-bottom: 4px; }}
  .subtitle {{ color: #666; font-size: 14px; margin-bottom: 30px; }}
  input[type=text] {{ font-size: 28px; text-align: center; width: 220px; padding: 12px;
                      border: 2px solid #ddd; border-radius: 8px; letter-spacing: 4px;
                      text-transform: uppercase; }}
  input[type=text]:focus {{ border-color: #4a90d9; outline: none; }}
  button {{ padding: 12px 40px; font-size: 16px; background: #4a90d9; color: white;
            border: none; border-radius: 8px; cursor: pointer; margin-top: 20px; }}
  button:hover {{ background: #357abd; }}
  .hint {{ color: #999; font-size: 12px; margin-top: 30px; }}
</style></head>
<body>
  <h2>Home-Still Cloud</h2>
  <p class="subtitle">Authorize <strong>{client_name}</strong> to access your research pipeline.<br>
  After you authorize you will be sent to <strong>{returns_to}</strong>.</p>
  <form method="POST" action="/authorize">
    <input type="hidden" name="response_type" value="code">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="S256">
    <input type="text" name="enrollment_code" placeholder="ABC-DEF"
           maxlength="7" autofocus autocomplete="off">
    <br>
    <button type="submit">Authorize</button>
  </form>
  <p class="hint">Generate a code: <code>hs cloud invite</code></p>
</body>
</html>"#,
            client_name = html_escape(&req.client.client_name),
            returns_to = html_escape(&returns_to),
            client_id = html_escape(&req.client.client_id),
            redirect_uri = html_escape(&req.redirect_uri),
            state = html_escape(&req.state),
            code_challenge = html_escape(&req.code_challenge),
        ),
    )
}

#[derive(Deserialize)]
pub struct AuthorizeForm {
    #[serde(flatten)]
    params: AuthorizeParams,
    enrollment_code: String,
}

/// `redirect_uri` with `code` (and `state`, when the client sent one) appended
/// as properly percent-encoded query parameters.
fn redirect_location(redirect_uri: &str, code: &str, state: &str) -> Result<HeaderValue, String> {
    let mut url = Url::parse(redirect_uri).map_err(|e| e.to_string())?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("code", code);
        if !state.is_empty() {
            query.append_pair("state", state);
        }
    }
    HeaderValue::from_str(url.as_str()).map_err(|e| e.to_string())
}

/// POST /authorize — validate enrollment code, redirect with auth code
pub async fn handle_authorize_post(
    State(state): State<Arc<GatewayState>>,
    axum::Form(form): axum::Form<AuthorizeForm>,
) -> Response {
    // Validate the request BEFORE touching the enrollment code: a malformed or
    // hostile request must neither redirect anywhere nor burn the user's code.
    let req = match validate_authorize(&state, &form.params) {
        Ok(r) => r,
        Err(why) => {
            return error_page(
                StatusCode::BAD_REQUEST,
                "Invalid authorization request",
                why,
            )
        }
    };

    let auth_code = generate_auth_code();
    let location = match redirect_location(&req.redirect_uri, &auth_code, &req.state) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot build redirect for a registered redirect_uri: {e}");
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Server error",
                "Could not complete the authorization.",
            );
        }
    };

    // Reserve the auth-code slot first; the single-use enrollment code is only
    // consumed once the slot is held, so a full store never burns it.
    let normalized = normalize_code(&form.enrollment_code);
    let (client_id, redirect_uri, code_challenge) =
        (req.client.client_id, req.redirect_uri, req.code_challenge);
    let inserted = state.auth_codes.insert_with(auth_code, || {
        let enrollment = state.enrollments.take(&normalized)?;
        Some(PendingAuthCode {
            client_id,
            redirect_uri,
            code_challenge,
            scopes: enrollment.scopes,
        })
    });
    match inserted {
        Ok(true) => {}
        Ok(false) => {
            return error_page(
                StatusCode::UNAUTHORIZED,
                "Invalid Code",
                "The enrollment code was invalid or expired. Generate a new one with `hs cloud invite`, then go back and try again.",
            );
        }
        Err(_) => {
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Busy",
                "Too many authorizations are in progress. Try again in a minute.",
            );
        }
    }

    let mut resp = StatusCode::SEE_OTHER.into_response();
    resp.headers_mut().insert(header::LOCATION, location);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// ── Token endpoint ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    scope: String,
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::error!("token creation failed: {e}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "token creation failed",
    )
}

/// POST /token — exchange auth code for tokens, or refresh
pub async fn handle_token(
    State(state): State<Arc<GatewayState>>,
    axum::Form(req): axum::Form<TokenRequest>,
) -> Response {
    match req.grant_type.as_str() {
        "authorization_code" => handle_code_exchange(state, req).await,
        "refresh_token" => handle_refresh(state, req).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "unsupported grant_type",
        ),
    }
}

async fn handle_code_exchange(state: Arc<GatewayState>, req: TokenRequest) -> Response {
    let Some(code) = req.code.as_deref() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "missing code");
    };
    let Some(verifier) = req.code_verifier.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing code_verifier",
        );
    };

    // Look up and consume the authorization code
    let Some(pending) = state.auth_codes.take(code) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "invalid or expired code",
        );
    };

    // Verify client_id and redirect_uri match
    if req.client_id.as_deref() != Some(&pending.client_id) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "client_id mismatch",
        );
    }
    if req.redirect_uri.as_deref() != Some(&pending.redirect_uri) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri mismatch",
        );
    }

    // Verify PKCE: SHA256(code_verifier) must match code_challenge
    if !verify_pkce_s256(verifier, &pending.code_challenge) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "PKCE verification failed",
        );
    }

    // Issue tokens carrying exactly the scopes the enrollment code granted.
    let sub = format!("oauth:{}", pending.client_id);
    let access_token = match auth::issue_token(&state, &sub, &pending.scopes, TokenType::Access) {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };
    let refresh_token = match auth::issue_token(&state, &sub, &pending.scopes, TokenType::Refresh) {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };

    Json(TokenResponse {
        access_token,
        token_type: "Bearer".into(),
        expires_in: state.config.token_ttl_secs,
        refresh_token: Some(refresh_token),
        scope: "mcp:tools".into(),
    })
    .into_response()
}

async fn handle_refresh(state: Arc<GatewayState>, req: TokenRequest) -> Response {
    let Some(refresh) = req.refresh_token.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing refresh_token",
        );
    };

    // Only refresh tokens may be exchanged here — an access token must not be
    // able to mint fresh ones.
    let claims = match auth::authenticate_token(&state, refresh, TokenType::Refresh) {
        Ok(c) => c,
        Err(_) => {
            return oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_grant",
                "invalid or expired refresh token",
            );
        }
    };

    let access_token =
        match auth::issue_token(&state, &claims.sub, &claims.scope, TokenType::Access) {
            Ok(t) => t,
            Err(e) => return server_error(e),
        };

    Json(TokenResponse {
        access_token,
        token_type: "Bearer".into(),
        expires_in: state.config.token_ttl_secs,
        refresh_token: None,
        scope: "mcp:tools".into(),
    })
    .into_response()
}

// ── Dynamic Client Registration ────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    client_name: Option<String>,
    redirect_uris: Option<Vec<String>>,
}

/// A redirect URI a client may register: https, or http on a loopback host
/// (native apps), with no credentials and no fragment. Anything else —
/// `javascript:`, `data:`, plain-http remote hosts — is refused up front, so
/// the authorization endpoint only ever redirects to one of these.
fn validate_redirect_uri(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() || raw.len() > MAX_REDIRECT_URI_LEN {
        return Err("redirect_uri has an invalid length");
    }
    if raw.chars().any(|c| c.is_control()) {
        return Err("redirect_uri contains control characters");
    }
    let url = Url::parse(raw).map_err(|_| "redirect_uri is not an absolute URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("redirect_uri must not contain credentials");
    }
    if url.fragment().is_some() {
        return Err("redirect_uri must not contain a fragment");
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => Ok(()),
        ("http", Some(Host::Ipv4(ip))) if ip.is_loopback() => Ok(()),
        ("http", Some(Host::Ipv6(ip))) if ip.is_loopback() => Ok(()),
        ("http", Some(Host::Domain("localhost"))) => Ok(()),
        _ => Err("redirect_uri must be https, or http on a loopback host"),
    }
}

/// POST /register — dynamic client registration (RFC 7591)
pub async fn handle_register(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let client_name = req.client_name.unwrap_or_else(|| "Unknown Client".into());
    if client_name.chars().count() > MAX_CLIENT_NAME_LEN
        || client_name.chars().any(|c| c.is_control())
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "client_name is too long or contains control characters",
        );
    }

    let redirect_uris = req.redirect_uris.unwrap_or_default();
    if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "between 1 and 10 redirect_uris are required",
        );
    }
    for uri in &redirect_uris {
        if let Err(why) = validate_redirect_uri(uri) {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", why);
        }
    }

    let client_id = generate_client_id();
    state.oauth_clients.insert_evicting_oldest(
        client_id.clone(),
        RegisteredClient {
            client_id: client_id.clone(),
            client_name: client_name.clone(),
            redirect_uris: redirect_uris.clone(),
        },
    );

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "client_id": client_id,
            "client_name": client_name,
            "redirect_uris": redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

// ── Helpers ────────────────────────────────────────────────────

fn random_alphanumeric(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..36u8);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect()
}

fn generate_auth_code() -> String {
    random_alphanumeric(32)
}

fn generate_client_id() -> String {
    format!("hs-{}", random_alphanumeric(16))
}

/// Verify PKCE S256: base64url(SHA256(verifier)) == challenge
fn verify_pkce_s256(verifier: &str, challenge: &str) -> bool {
    let hash = Sha256::digest(verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(hash);
    computed == challenge
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{body_json, body_string, call, test_state};
    use axum::body::Body;
    use axum::http::{Method, Request};

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const REDIRECT: &str = "https://claude.example.com/api/mcp/auth_callback";

    #[test]
    fn pkce_s256_verification() {
        // Known test vector
        assert!(verify_pkce_s256(VERIFIER, CHALLENGE));
    }

    #[test]
    fn pkce_wrong_verifier_rejected() {
        assert!(!verify_pkce_s256("wrong-verifier", CHALLENGE));
    }

    #[test]
    fn auth_code_generation() {
        let code = generate_auth_code();
        assert_eq!(code.len(), 32);
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn client_id_generation() {
        let id = generate_client_id();
        assert!(id.starts_with("hs-"));
        assert_eq!(id.len(), 19); // "hs-" + 16 chars
    }

    #[test]
    fn html_escape_neutralizes_markup_and_quotes() {
        let escaped = html_escape(r#""><script>alert('x')&</script>"#);
        assert_eq!(
            escaped,
            "&quot;&gt;&lt;script&gt;alert(&#x27;x&#x27;)&amp;&lt;/script&gt;"
        );
    }

    #[test]
    fn redirect_uris_are_limited_to_https_and_loopback_http() {
        for ok in [
            REDIRECT,
            "http://localhost:8080/callback",
            "http://127.0.0.1:33418/cb",
            "http://[::1]:9000/cb",
            "https://example.com/cb?x=1",
        ] {
            assert!(validate_redirect_uri(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "http://evil.example.com/cb",
            "https://user:pw@example.com/cb",
            "https://example.com/cb#frag",
            "https://example.com/cb\nSet-Cookie: x=1",
            "/relative/path",
            "myapp://callback",
        ] {
            assert!(validate_redirect_uri(bad).is_err(), "{bad:?}");
        }
    }

    // ── endpoint tests ─────────────────────────────────────────

    fn form<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            s.append_pair(k.as_ref(), v.as_ref());
        }
        s.finish()
    }

    fn post_form<K: AsRef<str>, V: AsRef<str>>(path: &str, pairs: &[(K, V)]) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form(pairs)))
            .unwrap()
    }

    async fn register_client(
        state: &Arc<GatewayState>,
        name: &str,
        redirect_uris: &[&str],
    ) -> String {
        let resp = call(
            state,
            crate::testutil::json_request(
                Method::POST,
                "/register",
                &serde_json::json!({ "client_name": name, "redirect_uris": redirect_uris }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        body_json(resp).await["client_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn invite(state: &Arc<GatewayState>, scopes: &[&str]) -> String {
        crate::enrollment::register_enrollment(
            &state.enrollments,
            "claude",
            scopes.iter().map(|s| s.to_string()).collect(),
        )
        .unwrap()
    }

    type Pairs = Vec<(String, String)>;

    /// Form fields for a valid authorize request.
    fn authorize_pairs(client_id: &str, code: &str) -> Pairs {
        [
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT),
            ("state", "st"),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("enrollment_code", code),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn replace(mut pairs: Pairs, key: &str, value: &str) -> Pairs {
        pairs.retain(|(k, _)| k != key);
        pairs.push((key.to_string(), value.to_string()));
        pairs
    }

    fn without(mut pairs: Pairs, key: &str) -> Pairs {
        pairs.retain(|(k, _)| k != key);
        pairs
    }

    #[tokio::test]
    async fn dynamic_registration_refuses_unsafe_redirect_uris() {
        let state = test_state(&[]).await;
        for bad in [
            "javascript:alert(1)",
            "http://evil.example.com/cb",
            "https://a.example/cb\nx",
        ] {
            let resp = call(
                &state,
                crate::testutil::json_request(
                    Method::POST,
                    "/register",
                    &serde_json::json!({ "redirect_uris": [bad] }),
                ),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        }
        let none = call(
            &state,
            crate::testutil::json_request(Method::POST, "/register", &serde_json::json!({})),
        )
        .await;
        assert_eq!(none.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authorize_form_escapes_every_interpolated_value() {
        let state = test_state(&[]).await;
        let payload = r#""><script>alert(1)</script>"#;
        let redirect = format!("https://claude.example.com/cb?x={payload}");
        let client_id = register_client(&state, payload, &[&redirect]).await;

        let query = form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect),
            ("state", payload),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
        ]);
        let resp = call(
            &state,
            Request::get(format!("/authorize?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        let html = body_string(resp).await;
        assert!(!html.contains("<script"), "{html}");
        assert!(
            html.contains("&lt;script&gt;"),
            "payload must be shown escaped"
        );
    }

    #[tokio::test]
    async fn authorize_get_rejects_unknown_client_and_unregistered_redirect() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let base = |client: &str, redirect: &str| {
            form(&[
                ("response_type", "code"),
                ("client_id", client),
                ("redirect_uri", redirect),
                ("state", "s"),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
            ])
        };
        for (client, redirect) in [
            ("hs-unknown", REDIRECT),
            (client_id.as_str(), "https://attacker.example/steal"),
            // prefix / suffix tricks are not exact matches
            (
                client_id.as_str(),
                "https://claude.example.com/api/mcp/auth_callback/",
            ),
            (
                client_id.as_str(),
                "https://claude.example.com/api/mcp/auth_callback?x=1",
            ),
        ] {
            let resp = call(
                &state,
                Request::get(format!("/authorize?{}", base(client, redirect)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{client} {redirect}"
            );
            assert!(!body_string(resp).await.contains("<form"));
        }
    }

    #[tokio::test]
    async fn authorize_post_requires_a_registered_redirect_uri_and_does_not_burn_the_code() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let code = invite(&state, &["mcp"]);

        let pairs = replace(
            authorize_pairs(&client_id, &code),
            "redirect_uri",
            "https://attacker.example/steal",
        );
        let resp = call(&state, post_form("/authorize", &pairs)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get("location").is_none());
        // The enrollment code is still usable for a legitimate request.
        assert_eq!(state.enrollments.len(), 1);
    }

    #[tokio::test]
    async fn authorize_post_with_a_full_auth_code_store_does_not_burn_the_code() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        for i in 0..MAX_AUTH_CODES {
            let pending = PendingAuthCode {
                client_id: "x".into(),
                redirect_uri: REDIRECT.into(),
                code_challenge: CHALLENGE.into(),
                scopes: vec![],
            };
            state
                .auth_codes
                .insert(format!("fill{i}"), pending)
                .unwrap();
        }
        let code = invite(&state, &["mcp"]);

        let resp = call(
            &state,
            post_form("/authorize", &authorize_pairs(&client_id, &code)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().get("location").is_none());
        assert_eq!(state.enrollments.len(), 1, "enrollment code must survive");
    }

    #[tokio::test]
    async fn authorize_post_requires_s256_pkce() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let code = invite(&state, &["mcp"]);

        let cases: Vec<Pairs> = vec![
            replace(
                authorize_pairs(&client_id, &code),
                "code_challenge_method",
                "plain",
            ),
            without(authorize_pairs(&client_id, &code), "code_challenge_method"),
            without(authorize_pairs(&client_id, &code), "code_challenge"),
            replace(
                authorize_pairs(&client_id, &code),
                "code_challenge",
                "short",
            ),
            replace(authorize_pairs(&client_id, &code), "response_type", "token"),
        ];
        for pairs in cases {
            let resp = call(&state, post_form("/authorize", &pairs)).await;
            assert!(
                resp.status().is_client_error(),
                "{pairs:?} -> {}",
                resp.status()
            );
            assert!(resp.headers().get("location").is_none(), "{pairs:?}");
        }
        assert_eq!(
            state.enrollments.len(),
            1,
            "no attempt may consume the code"
        );
    }

    #[tokio::test]
    async fn crlf_in_state_is_percent_encoded_not_injected_and_never_panics() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let code = invite(&state, &["mcp"]);

        let hostile = "ok\r\nSet-Cookie: pwn=1\n&code=evil#frag";
        let pairs = replace(authorize_pairs(&client_id, &code), "state", hostile);
        let resp = call(&state, post_form("/authorize", &pairs)).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().get("set-cookie").is_none());
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert!(location.starts_with(REDIRECT), "{location}");
        assert!(!location.contains('\n') && !location.contains('\r'));
        assert!(!location.contains("evil#frag"));
        let url = Url::parse(location).unwrap();
        let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(params.iter().filter(|(k, _)| k == "code").count(), 1);
        assert_eq!(
            params.iter().find(|(k, _)| k == "state").unwrap().1,
            hostile,
            "state round-trips intact through encoding"
        );

        // A newline smuggled in as redirect_uri is not a registered URI.
        let code = invite(&state, &["mcp"]);
        let pairs = replace(
            authorize_pairs(&client_id, &code),
            "redirect_uri",
            "https://claude.example.com/cb\nSet-Cookie: x=1",
        );
        let resp = call(&state, post_form("/authorize", &pairs)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Run the full code flow, returning the token endpoint's JSON.
    async fn authorize_and_exchange(
        state: &Arc<GatewayState>,
        client_id: &str,
        scopes: &[&str],
    ) -> serde_json::Value {
        let code = invite(state, scopes);
        let resp = call(
            state,
            post_form("/authorize", &authorize_pairs(client_id, &code)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let auth_code = Url::parse(&location)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned();

        let resp = call(
            state,
            post_form(
                "/token",
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &auth_code),
                    ("code_verifier", VERIFIER),
                    ("client_id", client_id),
                    ("redirect_uri", REDIRECT),
                ],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    #[tokio::test]
    async fn oauth_tokens_carry_the_invite_scopes_and_the_right_types() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let tokens = authorize_and_exchange(&state, &client_id, &["mcp"]).await;

        let access = tokens["access_token"].as_str().unwrap();
        let refresh = tokens["refresh_token"].as_str().unwrap();
        let access_claims = auth::authenticate_token(&state, access, TokenType::Access).unwrap();
        let refresh_claims = auth::authenticate_token(&state, refresh, TokenType::Refresh).unwrap();
        assert_eq!(access_claims.scope, vec!["mcp".to_string()]);
        assert_eq!(refresh_claims.scope, vec!["mcp".to_string()]);
        assert_eq!(access_claims.sub, format!("oauth:{client_id}"));
        // Not the old hard-coded grant.
        assert!(!access_claims.has_scope("scribe"));
        assert!(!access_claims.has_scope("distill"));
        // Classes are not interchangeable.
        assert!(auth::authenticate_token(&state, refresh, TokenType::Access).is_err());
        assert!(auth::authenticate_token(&state, access, TokenType::Refresh).is_err());
    }

    #[tokio::test]
    async fn token_refresh_grant_accepts_only_refresh_tokens() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;
        let tokens = authorize_and_exchange(&state, &client_id, &["scribe", "mcp"]).await;
        let access = tokens["access_token"].as_str().unwrap().to_string();
        let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

        let denied = call(
            &state,
            post_form(
                "/token",
                &[("grant_type", "refresh_token"), ("refresh_token", &access)],
            ),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let ok = call(
            &state,
            post_form(
                "/token",
                &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
            ),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let minted = body_json(ok).await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let claims = auth::authenticate_token(&state, &minted, TokenType::Access).unwrap();
        assert_eq!(claims.scope, vec!["scribe".to_string(), "mcp".to_string()]);
    }

    #[tokio::test]
    async fn authorization_code_is_single_use_and_bound_to_pkce_and_client() {
        let state = test_state(&[]).await;
        let client_id = register_client(&state, "c", &[REDIRECT]).await;

        let issue_code = || async {
            let code = invite(&state, &["mcp"]);
            let resp = call(
                &state,
                post_form("/authorize", &authorize_pairs(&client_id, &code)),
            )
            .await;
            let location = resp
                .headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            Url::parse(&location)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "code")
                .unwrap()
                .1
                .into_owned()
        };
        let exchange = |code: String, verifier: &'static str| {
            let state = state.clone();
            let client_id = client_id.clone();
            async move {
                call(
                    &state,
                    post_form(
                        "/token",
                        &[
                            ("grant_type", "authorization_code"),
                            ("code", &code),
                            ("code_verifier", verifier),
                            ("client_id", &client_id),
                            ("redirect_uri", REDIRECT),
                        ],
                    ),
                )
                .await
                .status()
            }
        };

        // Wrong verifier fails, and burns the code.
        let code = issue_code().await;
        assert_eq!(
            exchange(code.clone(), "not-the-verifier").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(exchange(code, VERIFIER).await, StatusCode::BAD_REQUEST);

        // Replay after success fails.
        let code = issue_code().await;
        assert_eq!(exchange(code.clone(), VERIFIER).await, StatusCode::OK);
        assert_eq!(exchange(code, VERIFIER).await, StatusCode::BAD_REQUEST);
    }
}
