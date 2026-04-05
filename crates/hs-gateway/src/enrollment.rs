//! Device enrollment and token refresh endpoints.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use hs_common::auth::token::{self, TokenClaims, TokenError};

use crate::state::GatewayState;

/// A pending enrollment code with expiry.
pub struct PendingEnrollment {
    pub code: String,
    pub device_name: String,
    pub scopes: Vec<String>,
    pub created_at: Instant,
}

/// Thread-safe store for pending enrollment codes.
pub type EnrollmentStore = Arc<Mutex<HashMap<String, PendingEnrollment>>>;

pub fn new_enrollment_store() -> EnrollmentStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Register a new enrollment code (called from `hs cloud invite`).
pub fn register_enrollment(
    store: &EnrollmentStore,
    device_name: &str,
    scopes: Vec<String>,
) -> String {
    let code = token::generate_enrollment_code();
    let enrollment = PendingEnrollment {
        code: code.clone(),
        device_name: device_name.into(),
        scopes,
        created_at: Instant::now(),
    };
    let mut guard = store.lock().unwrap();
    // Clean up expired codes while we're here
    guard.retain(|_, e| e.created_at.elapsed().as_secs() < 300);
    guard.insert(code.clone(), enrollment);
    code
}

// ── HTTP Handlers ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EnrollRequest {
    code: String,
    device_name: Option<String>,
}

#[derive(Serialize)]
pub struct EnrollResponse {
    refresh_token: String,
    device_name: String,
    gateway_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cf_access_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cf_access_client_secret: Option<String>,
}

/// POST /cloud/enroll — exchange an enrollment code for a refresh token.
pub async fn handle_enroll(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<EnrollRequest>,
) -> impl IntoResponse {
    let code = req.code.trim().to_uppercase();

    // Look up and consume the enrollment code
    let enrollment = {
        let mut guard = state.enrollments.lock().unwrap();
        guard.remove(&code)
    };

    let enrollment = match enrollment {
        Some(e) => {
            if e.created_at.elapsed().as_secs() > 300 {
                return (StatusCode::GONE, "Enrollment code expired").into_response();
            }
            e
        }
        None => {
            return (StatusCode::UNAUTHORIZED, "Invalid enrollment code").into_response();
        }
    };

    let device_name = req
        .device_name
        .unwrap_or_else(|| enrollment.device_name.clone());

    // Create a refresh token
    let claims = TokenClaims {
        sub: device_name.clone(),
        iat: token::now_epoch(),
        exp: token::now_epoch() + state.config.refresh_ttl_secs,
        scope: enrollment.scopes,
    };

    let refresh_token = match token::create_token(&state.secret, &claims) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Token creation failed: {e}"),
            )
                .into_response();
        }
    };

    Json(EnrollResponse {
        refresh_token,
        device_name,
        gateway_url: state.gateway_url.clone(),
        cf_access_client_id: state.cf_access_client_id.clone(),
        cf_access_client_secret: state.cf_access_client_secret.clone(),
    })
    .into_response()
}

/// POST /cloud/refresh — exchange a refresh token for an access token.
pub async fn handle_refresh(
    State(state): State<Arc<GatewayState>>,
    req: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let refresh_token = match auth_header {
        Some(t) => t,
        None => {
            return (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response();
        }
    };

    // Validate refresh token
    let claims = match token::validate_token(&state.secret, refresh_token, false) {
        Ok(c) => c,
        Err(TokenError::Expired) => {
            return (
                StatusCode::UNAUTHORIZED,
                "Refresh token expired — re-enroll with `hs cloud enroll`",
            )
                .into_response();
        }
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, "Invalid refresh token").into_response();
        }
    };

    // Issue a short-lived access token with the same scopes
    let access_claims = TokenClaims {
        sub: claims.sub,
        iat: token::now_epoch(),
        exp: token::now_epoch() + state.config.token_ttl_secs,
        scope: claims.scope,
    };

    let access_token = match token::create_token(&state.secret, &access_claims) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Token creation failed: {e}"),
            )
                .into_response();
        }
    };

    #[derive(Serialize)]
    struct RefreshResponse {
        access_token: String,
    }

    Json(RefreshResponse { access_token }).into_response()
}

// ── Admin endpoints (localhost only) ───────────────────────────

#[derive(Deserialize)]
pub struct AdminInviteRequest {
    device_name: String,
    #[serde(default = "default_scopes")]
    scopes: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    vec!["scribe".into(), "distill".into()]
}

#[derive(Serialize)]
pub struct AdminInviteResponse {
    code: String,
    expires_in_secs: u64,
}

/// POST /cloud/admin/invite — create an enrollment code (admin, localhost only).
pub async fn handle_admin_invite(
    State(state): State<Arc<GatewayState>>,
    req: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    // Only allow from localhost
    let is_local = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(true); // if no ConnectInfo, assume behind reverse proxy (localhost)

    if !is_local {
        return (StatusCode::FORBIDDEN, "Admin endpoints are localhost-only").into_response();
    }

    let body = match axum::body::to_bytes(req.into_body(), 4096).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid body").into_response(),
    };

    let invite_req: AdminInviteRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response(),
    };

    let code = register_enrollment(
        &state.enrollments,
        &invite_req.device_name,
        invite_req.scopes,
    );

    Json(AdminInviteResponse {
        code,
        expires_in_secs: 300,
    })
    .into_response()
}
