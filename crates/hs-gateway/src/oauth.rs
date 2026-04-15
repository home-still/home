//! OAuth 2.1 Authorization Code + PKCE flow for Claude Desktop MCP access.
//!
//! Implements the endpoints Claude Desktop needs to authenticate with the gateway:
//! - Well-known discovery endpoints (RFC 8414, RFC 9728)
//! - Authorization endpoint (enrollment code form)
//! - Token endpoint (code exchange + PKCE verification + refresh)
//! - Dynamic Client Registration (RFC 7591)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use hs_common::auth::token;

use crate::state::GatewayState;

// ── In-memory stores ───────────────────────────────────────────

/// A pending OAuth authorization code waiting to be exchanged.
pub struct PendingAuthCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
    created_at: Instant,
}

/// A dynamically registered OAuth client.
#[derive(Clone, Serialize)]
pub struct RegisteredClient {
    client_id: String,
    client_name: String,
    redirect_uris: Vec<String>,
}

pub type AuthCodeStore = Arc<Mutex<HashMap<String, PendingAuthCode>>>;
pub type ClientStore = Arc<Mutex<HashMap<String, RegisteredClient>>>;

pub fn new_auth_code_store() -> AuthCodeStore {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn new_client_store() -> ClientStore {
    Arc::new(Mutex::new(HashMap::new()))
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

#[derive(Deserialize)]
pub struct AuthorizeParams {
    client_id: Option<String>,
    redirect_uri: Option<String>,
    response_type: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

/// GET /authorize — show enrollment code form
pub async fn handle_authorize_get(Query(params): Query<AuthorizeParams>) -> Html<String> {
    let client_id = params.client_id.unwrap_or_default();
    let redirect_uri = params.redirect_uri.unwrap_or_default();
    let state = params.state.unwrap_or_default();
    let code_challenge = params.code_challenge.unwrap_or_default();
    let code_challenge_method = params.code_challenge_method.unwrap_or_default();
    let scope = params.scope.unwrap_or_default();

    Html(format!(
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
  .error {{ color: #d33; margin-bottom: 15px; }}
</style></head>
<body>
  <h2>Home-Still Cloud</h2>
  <p class="subtitle">Authorize access to your research pipeline</p>
  <form method="POST" action="/authorize">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="{code_challenge_method}">
    <input type="hidden" name="scope" value="{scope}">
    <input type="text" name="enrollment_code" placeholder="ABC-DEF"
           maxlength="7" autofocus autocomplete="off">
    <br>
    <button type="submit">Authorize</button>
  </form>
  <p class="hint">Generate a code: <code>hs cloud invite</code></p>
</body>
</html>"#
    ))
}

#[derive(Deserialize)]
pub struct AuthorizeForm {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    scope: String,
    enrollment_code: String,
}

/// POST /authorize — validate enrollment code, redirect with auth code
pub async fn handle_authorize_post(
    State(state): State<Arc<GatewayState>>,
    axum::Form(form): axum::Form<AuthorizeForm>,
) -> Response {
    let code_input = form.enrollment_code.trim().to_uppercase();

    // Validate enrollment code against the enrollment store
    let enrollment = {
        let mut guard = state.enrollments.lock().unwrap();
        guard.remove(&code_input)
    };

    match enrollment {
        Some(e) if e.created_at.elapsed().as_secs() <= 300 => {
            // Valid enrollment code — generate OAuth authorization code
            let auth_code = generate_auth_code();

            let pending = PendingAuthCode {
                client_id: form.client_id,
                redirect_uri: form.redirect_uri.clone(),
                code_challenge: form.code_challenge,
                scope: form.scope,
                created_at: Instant::now(),
            };

            state
                .auth_codes
                .lock()
                .unwrap()
                .insert(auth_code.clone(), pending);

            // Redirect to the client's callback with the auth code
            let redirect_url = format!(
                "{}?code={}&state={}",
                form.redirect_uri, auth_code, form.state
            );
            Redirect::to(&redirect_url).into_response()
        }
        _ => {
            // Invalid or expired — show error page
            Html(
                r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Error</title>
<style>body { font-family: system-ui; max-width: 400px; margin: 80px auto; text-align: center; }</style>
</head><body>
<h2>Invalid Code</h2>
<p>The enrollment code was invalid or expired.</p>
<p>Generate a new one: <code>hs cloud invite</code></p>
<p><a href="javascript:history.back()">Try again</a></p>
</body></html>"#
                    .to_string(),
            )
            .into_response()
        }
    }
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

/// POST /token — exchange auth code for tokens, or refresh
pub async fn handle_token(
    State(state): State<Arc<GatewayState>>,
    axum::Form(req): axum::Form<TokenRequest>,
) -> Response {
    match req.grant_type.as_str() {
        "authorization_code" => handle_code_exchange(state, req).await,
        "refresh_token" => handle_refresh(state, req).await,
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "unsupported_grant_type" })),
        )
            .into_response(),
    }
}

async fn handle_code_exchange(state: Arc<GatewayState>, req: TokenRequest) -> Response {
    let code = match &req.code {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid_request", "error_description": "missing code" })),
            )
                .into_response()
        }
    };

    let verifier = match &req.code_verifier {
        Some(v) => v.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid_request", "error_description": "missing code_verifier" })),
            )
                .into_response()
        }
    };

    // Look up and consume the authorization code
    let pending = {
        let mut guard = state.auth_codes.lock().unwrap();
        guard.remove(&code)
    };

    let pending = match pending {
        Some(p) if p.created_at.elapsed().as_secs() <= 60 => p,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid_grant", "error_description": "invalid or expired code" })),
            )
                .into_response()
        }
    };

    // Verify client_id and redirect_uri match
    if req.client_id.as_deref() != Some(&pending.client_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_grant", "error_description": "client_id mismatch" })),
        )
            .into_response();
    }
    if req.redirect_uri.as_deref() != Some(&pending.redirect_uri) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_grant", "error_description": "redirect_uri mismatch" })),
        )
            .into_response();
    }

    // Verify PKCE: SHA256(code_verifier) must match code_challenge
    if !verify_pkce_s256(&verifier, &pending.code_challenge) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_grant", "error_description": "PKCE verification failed" })),
        )
            .into_response();
    }

    // Issue tokens
    let scopes = vec!["scribe".into(), "distill".into(), "mcp".into()];

    let access_claims = token::TokenClaims {
        sub: format!("oauth:{}", pending.client_id),
        iat: token::now_epoch(),
        exp: token::now_epoch() + state.config.token_ttl_secs,
        scope: scopes.clone(),
    };

    let refresh_claims = token::TokenClaims {
        sub: format!("oauth:{}", pending.client_id),
        iat: token::now_epoch(),
        exp: token::now_epoch() + state.config.refresh_ttl_secs,
        scope: scopes,
    };

    let access_token = match token::create_token(&state.secret, &access_claims) {
        Ok(t) => t,
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::json!({ "error": "server_error", "error_description": format!("{e}") }),
            ),
        )
            .into_response(),
    };

    let refresh_token = match token::create_token(&state.secret, &refresh_claims) {
        Ok(t) => t,
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::json!({ "error": "server_error", "error_description": format!("{e}") }),
            ),
        )
            .into_response(),
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
    let refresh = match &req.refresh_token {
        Some(t) => t.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid_request", "error_description": "missing refresh_token" })),
            )
                .into_response()
        }
    };

    let claims = match token::validate_token(&state.secret, &refresh, false) {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid_grant", "error_description": "invalid or expired refresh token" })),
            )
                .into_response()
        }
    };

    let access_claims = token::TokenClaims {
        sub: claims.sub,
        iat: token::now_epoch(),
        exp: token::now_epoch() + state.config.token_ttl_secs,
        scope: claims.scope,
    };

    let access_token = match token::create_token(&state.secret, &access_claims) {
        Ok(t) => t,
        Err(e) => return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                serde_json::json!({ "error": "server_error", "error_description": format!("{e}") }),
            ),
        )
            .into_response(),
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
    #[allow(dead_code)]
    grant_types: Option<Vec<String>>,
    #[allow(dead_code)]
    response_types: Option<Vec<String>>,
    #[allow(dead_code)]
    token_endpoint_auth_method: Option<String>,
}

/// POST /register — dynamic client registration (RFC 7591)
pub async fn handle_register(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let client_id = generate_client_id();
    let client_name = req.client_name.unwrap_or_else(|| "Unknown Client".into());
    let redirect_uris = req.redirect_uris.unwrap_or_default();

    let client = RegisteredClient {
        client_id: client_id.clone(),
        client_name: client_name.clone(),
        redirect_uris: redirect_uris.clone(),
    };

    state
        .oauth_clients
        .lock()
        .unwrap()
        .insert(client_id.clone(), client);

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

fn generate_auth_code() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| {
            let idx = rng.random_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect()
}

fn generate_client_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    format!(
        "hs-{}",
        (0..16)
            .map(|_| {
                let idx = rng.random_range(0..36);
                if idx < 10 {
                    (b'0' + idx) as char
                } else {
                    (b'a' + idx - 10) as char
                }
            })
            .collect::<String>()
    )
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

    #[test]
    fn pkce_s256_verification() {
        // Known test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce_s256(verifier, challenge));
    }

    #[test]
    fn pkce_wrong_verifier_rejected() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(!verify_pkce_s256("wrong-verifier", challenge));
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
}
