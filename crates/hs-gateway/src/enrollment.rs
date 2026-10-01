//! Device enrollment and token refresh endpoints.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hs_common::auth::token::{self, TokenType};
use serde::{Deserialize, Serialize};

use crate::auth::{self, AuthError};
use crate::state::GatewayState;
use crate::store::{ExpiringStore, Full};

/// How long an enrollment code stays valid.
pub const ENROLLMENT_TTL: Duration = Duration::from_secs(300);

/// Most enrollment codes that may be outstanding at once.
const MAX_PENDING_ENROLLMENTS: usize = 256;

/// A pending enrollment: what the admin who issued the code decided.
pub struct PendingEnrollment {
    pub device_name: String,
    pub scopes: Vec<String>,
}

/// Pending enrollment codes, keyed by code. Single use, expiring, bounded.
pub type EnrollmentStore = ExpiringStore<PendingEnrollment>;

pub fn new_enrollment_store() -> EnrollmentStore {
    ExpiringStore::new(ENROLLMENT_TTL, MAX_PENDING_ENROLLMENTS)
}

/// Register a new enrollment code (called from the admin invite endpoint).
pub fn register_enrollment(
    store: &EnrollmentStore,
    device_name: &str,
    scopes: Vec<String>,
) -> Result<String, Full> {
    let code = token::generate_enrollment_code();
    store.insert(
        code.clone(),
        PendingEnrollment {
            device_name: device_name.into(),
            scopes,
        },
    )?;
    Ok(code)
}

/// Normalize a code as typed by a human.
pub fn normalize_code(input: &str) -> String {
    input.trim().to_uppercase()
}

// ── HTTP Handlers ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EnrollRequest {
    code: String,
}

#[derive(Serialize)]
pub struct EnrollResponse {
    refresh_token: String,
    device_name: String,
}

/// POST /cloud/enroll — exchange an enrollment code for a refresh token.
///
/// The device name and scopes are whatever the administrator chose when the
/// code was issued; the enrolling device does not get to pick its own identity
/// (the registry's ownership checks key on it).
pub async fn handle_enroll(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<EnrollRequest>,
) -> Response {
    let Some(enrollment) = state.enrollments.take(&normalize_code(&req.code)) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or expired enrollment code",
        )
            .into_response();
    };

    let refresh_token = match auth::issue_token(
        &state,
        &enrollment.device_name,
        &enrollment.scopes,
        TokenType::Refresh,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("token creation failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Token creation failed").into_response();
        }
    };

    Json(EnrollResponse {
        refresh_token,
        device_name: enrollment.device_name,
    })
    .into_response()
}

/// POST /cloud/refresh — exchange a refresh token for an access token.
/// Only refresh tokens are accepted: an access token cannot mint new tokens.
pub async fn handle_refresh(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    let claims = match auth::authenticate(&state, &headers, TokenType::Refresh) {
        Ok(c) => c,
        Err(AuthError::Missing) => {
            return (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response();
        }
        Err(AuthError::Expired) => {
            return (
                StatusCode::UNAUTHORIZED,
                "Refresh token expired — re-enroll with `hs cloud enroll`",
            )
                .into_response();
        }
        Err(AuthError::Revoked) => {
            return (
                StatusCode::UNAUTHORIZED,
                "Refresh token revoked — re-enroll with `hs cloud enroll`",
            )
                .into_response();
        }
        Err(AuthError::WrongType) => {
            return (StatusCode::UNAUTHORIZED, "Not a refresh token").into_response();
        }
        Err(AuthError::Invalid) => {
            return (StatusCode::UNAUTHORIZED, "Invalid refresh token").into_response();
        }
    };

    // Issue a short-lived access token with the same subject and scopes.
    let access_token =
        match auth::issue_token(&state, &claims.sub, &claims.scope, TokenType::Access) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("token creation failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Token creation failed")
                    .into_response();
            }
        };

    #[derive(Serialize)]
    struct RefreshResponse {
        access_token: String,
    }

    Json(RefreshResponse { access_token }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{bearer_headers, body_json, call, json_request, test_state};
    use axum::http::Method;

    #[tokio::test]
    async fn enrolled_device_gets_the_identity_and_scopes_the_admin_chose() {
        let state = test_state(&[]).await;
        let code =
            register_enrollment(&state.enrollments, "laptop", vec!["scribe".into()]).unwrap();

        // A device name in the request body is ignored.
        let resp = call(
            &state,
            json_request(
                Method::POST,
                "/cloud/enroll",
                &serde_json::json!({ "code": code.to_lowercase(), "device_name": "big" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["device_name"], "laptop");

        let refresh = body["refresh_token"].as_str().unwrap();
        let claims = auth::authenticate_token(&state, refresh, TokenType::Refresh).unwrap();
        assert_eq!(claims.sub, "laptop");
        assert_eq!(claims.scope, vec!["scribe".to_string()]);
    }

    #[tokio::test]
    async fn enrollment_code_is_single_use() {
        let state = test_state(&[]).await;
        let code = register_enrollment(&state.enrollments, "laptop", vec!["mcp".into()]).unwrap();
        let req = || {
            json_request(
                Method::POST,
                "/cloud/enroll",
                &serde_json::json!({ "code": code }),
            )
        };
        assert_eq!(call(&state, req()).await.status(), StatusCode::OK);
        assert_eq!(call(&state, req()).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_accepts_refresh_tokens_and_rejects_access_tokens() {
        let state = test_state(&[]).await;
        let scope = vec!["scribe".to_string(), "mcp".to_string()];
        let refresh = auth::issue_token(&state, "laptop", &scope, TokenType::Refresh).unwrap();
        let access = auth::issue_token(&state, "laptop", &scope, TokenType::Access).unwrap();

        let ok = call(
            &state,
            axum::http::Request::post("/cloud/refresh")
                .header("authorization", format!("Bearer {refresh}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let minted = body_json(ok).await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let claims =
            auth::authenticate(&state, &bearer_headers(&minted), TokenType::Access).unwrap();
        assert_eq!(claims.sub, "laptop");
        assert_eq!(claims.scope, scope);

        // An access token must not be exchangeable for fresh tokens.
        let denied = call(
            &state,
            axum::http::Request::post("/cloud/refresh")
                .header("authorization", format!("Bearer {access}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_rejects_a_revoked_device() {
        let state = test_state(&[]).await;
        let refresh =
            auth::issue_token(&state, "laptop", &["mcp".to_string()], TokenType::Refresh).unwrap();
        state.revocations.revoke("laptop").unwrap();
        let denied = call(
            &state,
            axum::http::Request::post("/cloud/refresh")
                .header("authorization", format!("Bearer {refresh}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }
}
