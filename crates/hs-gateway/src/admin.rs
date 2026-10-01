//! Administrator endpoints: issue enrollment codes, revoke devices.
//!
//! These are reachable by the same route as everything else (cloudflared
//! delivers internet traffic from loopback), so nothing about the *connection*
//! can be trusted. Access requires the gateway's admin key, a secret distinct
//! from the token-signing secret, stored 0600 beside it and readable by the
//! operator on the gateway host (`hs cloud invite` reads it from there).
//! Requests that carry proxy/CDN headers are refused outright as defense in
//! depth: a legitimate admin call is a direct loopback connection from the CLI
//! and has none.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::auth::{self, Rejection, SERVICES};
use crate::enrollment::{self, ENROLLMENT_TTL};
use crate::state::GatewayState;

/// Largest admin request body.
pub const MAX_ADMIN_BODY: usize = 4096;

/// Longest device name accepted.
const MAX_DEVICE_NAME_LEN: usize = 64;

/// Headers that mean the request travelled through a proxy or CDN. The CLI
/// talks to the gateway directly, so none of these may appear on an admin call.
fn is_proxy_header(name: &str) -> bool {
    name.starts_with("cf-")
        || name.starts_with("x-forwarded-")
        || matches!(
            name,
            "forwarded" | "x-real-ip" | "true-client-ip" | "cdn-loop" | "via"
        )
}

/// Check the request is an authentic admin call.
fn authorize_admin(state: &GatewayState, headers: &HeaderMap) -> Result<(), Rejection> {
    if headers.keys().any(|name| is_proxy_header(name.as_str())) {
        return Err((
            StatusCode::FORBIDDEN,
            "Admin endpoints cannot be reached through a proxy",
        ));
    }
    let presented =
        auth::bearer(headers).ok_or((StatusCode::UNAUTHORIZED, "Admin key required"))?;
    let matches: bool = presented
        .as_bytes()
        .ct_eq(state.admin_key.as_bytes())
        .into();
    if !matches {
        return Err((StatusCode::UNAUTHORIZED, "Invalid admin key"));
    }
    Ok(())
}

/// Parse a JSON admin body. Size is capped by the route's `DefaultBodyLimit`
/// ([`MAX_ADMIN_BODY`]) before the handler runs.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Rejection> {
    serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "Invalid JSON"))
}

/// A device name becomes the token `sub` and a log field: keep it to a short,
/// boring alphabet. (`:` is excluded so it can never collide with the
/// `oauth:<client_id>` subjects.)
fn validate_device_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_DEVICE_NAME_LEN {
        return Err(format!(
            "device_name must be 1-{MAX_DEVICE_NAME_LEN} characters"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("device_name may contain only letters, digits, '-', '_' and '.'".into());
    }
    Ok(())
}

/// Only the three service scopes are grantable — never `*`, never anything
/// the proxy does not route.
fn validate_scopes(scopes: Vec<String>) -> Result<Vec<String>, String> {
    if scopes.is_empty() {
        return Err("scopes must not be empty".into());
    }
    let mut out: Vec<String> = Vec::new();
    for scope in scopes {
        if !SERVICES.contains(&scope.as_str()) {
            return Err(format!(
                "scope {scope:?} cannot be granted (allowed: {})",
                SERVICES.join(", ")
            ));
        }
        if !out.contains(&scope) {
            out.push(scope);
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct AdminInviteRequest {
    device_name: String,
    #[serde(default = "default_scopes")]
    scopes: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    SERVICES.iter().map(|s| s.to_string()).collect()
}

#[derive(Serialize)]
pub struct AdminInviteResponse {
    code: String,
    expires_in_secs: u64,
    scopes: Vec<String>,
}

/// POST /cloud/admin/invite — create an enrollment code.
pub async fn handle_admin_invite(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authorize_admin(&state, &headers) {
        return rejection.into_response();
    }
    let req: AdminInviteRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(rejection) => return rejection.into_response(),
    };
    if let Err(e) = validate_device_name(&req.device_name) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    let scopes = match validate_scopes(req.scopes) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    match enrollment::register_enrollment(&state.enrollments, &req.device_name, scopes.clone()) {
        Ok(code) => Json(AdminInviteResponse {
            code,
            expires_in_secs: ENROLLMENT_TTL.as_secs(),
            scopes,
        })
        .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many pending enrollment codes; wait for some to expire",
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct AdminRevokeRequest {
    /// Device name, or `oauth:<client_id>` for an OAuth client.
    subject: String,
}

#[derive(Serialize)]
pub struct AdminRevokeResponse {
    subject: String,
    revoked_at: u64,
    registry_entries_removed: usize,
}

/// POST /cloud/admin/revoke — invalidate every token issued to a subject up to
/// now and drop its registry entries.
pub async fn handle_admin_revoke(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authorize_admin(&state, &headers) {
        return rejection.into_response();
    }
    let req: AdminRevokeRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(rejection) => return rejection.into_response(),
    };
    if req.subject.is_empty() || req.subject.len() > 128 {
        return (StatusCode::BAD_REQUEST, "subject must be 1-128 characters").into_response();
    }

    let revoked_at = match state.revocations.revoke(&req.subject) {
        Ok(at) => at,
        Err(e) => {
            tracing::error!("persisting revocation for {}: {e:#}", req.subject);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not persist revocation",
            )
                .into_response();
        }
    };
    let removed = state.registry.remove_owned_by(&req.subject).await;
    tracing::info!(
        "revoked {:?} (removed {removed} registry entries)",
        req.subject
    );

    Json(AdminRevokeResponse {
        subject: req.subject,
        revoked_at,
        registry_entries_removed: removed,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{body_json, call, call_from, test_state};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use hs_common::auth::token::TokenType;

    const LOOPBACK: &str = "127.0.0.1:50000";

    fn invite(
        admin_key: Option<&str>,
        extra: &[(&str, &str)],
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/cloud/admin/invite")
            .header("content-type", "application/json");
        if let Some(key) = admin_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    fn all_scopes() -> serde_json::Value {
        serde_json::json!({ "device_name": "laptop", "scopes": ["scribe", "distill", "mcp"] })
    }

    #[tokio::test]
    async fn loopback_peer_without_the_admin_key_is_refused() {
        let state = test_state(&[]).await;
        // The peer address is loopback — exactly what cloudflared looks like.
        let resp = call_from(&state, invite(None, &[], all_scopes()), LOOPBACK).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Same with no ConnectInfo at all (the old `unwrap_or(true)` case).
        let resp = call(&state, invite(None, &[], all_scopes())).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = call_from(&state, invite(Some("wrong"), &[], all_scopes()), LOOPBACK).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state.enrollments.len(), 0, "no code may have been issued");
    }

    #[tokio::test]
    async fn a_signed_token_is_not_an_admin_credential() {
        let state = test_state(&[]).await;
        let token = auth::issue_token(
            &state,
            "laptop",
            &[
                "scribe".to_string(),
                "distill".to_string(),
                "mcp".to_string(),
            ],
            TokenType::Access,
        )
        .unwrap();
        let resp = call_from(&state, invite(Some(&token), &[], all_scopes()), LOOPBACK).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn requests_that_travelled_through_a_proxy_are_refused_even_with_the_key() {
        let state = test_state(&[]).await;
        let key = state.admin_key.clone();
        for header in [
            ("cf-connecting-ip", "203.0.113.9"),
            ("cf-ray", "abc"),
            ("x-forwarded-for", "203.0.113.9"),
            ("x-forwarded-proto", "https"),
            ("x-real-ip", "203.0.113.9"),
            ("forwarded", "for=203.0.113.9"),
        ] {
            let resp = call_from(
                &state,
                invite(Some(&key), &[header], all_scopes()),
                LOOPBACK,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{header:?}");
        }
        assert_eq!(state.enrollments.len(), 0);
    }

    #[tokio::test]
    async fn wildcard_and_unknown_scopes_cannot_be_granted() {
        let state = test_state(&[]).await;
        let key = state.admin_key.clone();
        for scopes in [
            serde_json::json!(["*"]),
            serde_json::json!(["scribe", "*"]),
            serde_json::json!(["admin"]),
            serde_json::json!([]),
        ] {
            let body = serde_json::json!({ "device_name": "laptop", "scopes": scopes });
            let resp = call_from(&state, invite(Some(&key), &[], body), LOOPBACK).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{scopes}");
        }
        assert_eq!(state.enrollments.len(), 0);
    }

    #[tokio::test]
    async fn bad_device_names_are_refused() {
        let state = test_state(&[]).await;
        let key = state.admin_key.clone();
        for name in ["", "oauth:hs-abc", "a b", "x/y", &"n".repeat(65)] {
            let body = serde_json::json!({ "device_name": name, "scopes": ["mcp"] });
            let resp = call_from(&state, invite(Some(&key), &[], body), LOOPBACK).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name:?}");
        }
    }

    #[tokio::test]
    async fn correct_key_and_allowed_scopes_issue_a_code_that_enrolls_with_those_scopes() {
        let state = test_state(&[]).await;
        let key = state.admin_key.clone();
        let body =
            serde_json::json!({ "device_name": "laptop", "scopes": ["scribe", "scribe", "mcp"] });
        let resp = call_from(&state, invite(Some(&key), &[], body), LOOPBACK).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let issued = body_json(resp).await;
        assert_eq!(issued["scopes"], serde_json::json!(["scribe", "mcp"]));
        assert_eq!(issued["expires_in_secs"], 300);

        let enrolled = call(
            &state,
            crate::testutil::json_request(
                Method::POST,
                "/cloud/enroll",
                &serde_json::json!({ "code": issued["code"] }),
            ),
        )
        .await;
        assert_eq!(enrolled.status(), StatusCode::OK);
        let refresh = body_json(enrolled).await["refresh_token"]
            .as_str()
            .unwrap()
            .to_string();
        let claims = auth::authenticate_token(&state, &refresh, TokenType::Refresh).unwrap();
        assert_eq!(claims.scope, vec!["scribe".to_string(), "mcp".to_string()]);
    }

    #[tokio::test]
    async fn revoke_requires_the_admin_key_and_kills_existing_tokens() {
        let state = test_state(&[]).await;
        let access =
            auth::issue_token(&state, "laptop", &["mcp".to_string()], TokenType::Access).unwrap();
        let revoke = |key: Option<&str>| {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/cloud/admin/revoke")
                .header("content-type", "application/json");
            if let Some(key) = key {
                req = req.header("authorization", format!("Bearer {key}"));
            }
            req.body(Body::from(
                serde_json::json!({ "subject": "laptop" }).to_string(),
            ))
            .unwrap()
        };

        let denied = call_from(&state, revoke(None), LOOPBACK).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(auth::authenticate_token(&state, &access, TokenType::Access).is_ok());

        let key = state.admin_key.clone();
        let ok = call_from(&state, revoke(Some(&key)), LOOPBACK).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(auth::authenticate_token(&state, &access, TokenType::Access).is_err());
    }
}
