//! The gateway's HTTP router.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{any, get, post};
use axum::Router;

use crate::state::GatewayState;
use crate::{admin, enrollment, oauth, proxy, ratelimit, registry};

pub fn build_router(state: Arc<GatewayState>) -> Router {
    Router::new()
        // OAuth 2.1 discovery endpoints
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::handle_protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::handle_auth_server_metadata),
        )
        // OAuth 2.1 authorization + token + registration
        .route(
            "/authorize",
            get(oauth::handle_authorize_get).post(oauth::handle_authorize_post),
        )
        .route("/token", post(oauth::handle_token))
        .route("/register", post(oauth::handle_register))
        // Unauthenticated endpoints
        .route("/health", get(handle_health))
        .route("/cloud/enroll", post(enrollment::handle_enroll))
        .route("/cloud/refresh", post(enrollment::handle_refresh))
        // Admin endpoints: require the admin key; refuse anything that came
        // through a proxy. Small bodies only.
        .route(
            "/cloud/admin/invite",
            post(admin::handle_admin_invite).layer(DefaultBodyLimit::max(admin::MAX_ADMIN_BODY)),
        )
        .route(
            "/cloud/admin/revoke",
            post(admin::handle_admin_revoke).layer(DefaultBodyLimit::max(admin::MAX_ADMIN_BODY)),
        )
        // Service registry
        .route("/registry/register", post(registry::handle_register))
        .route(
            "/registry/deregister",
            axum::routing::delete(registry::handle_deregister),
        )
        .route("/registry/heartbeat", post(registry::handle_heartbeat))
        .route("/registry/services", get(registry::handle_services))
        .route("/registry/set-enabled", post(registry::handle_set_enabled))
        // Authenticated proxy — catch all remaining paths
        .fallback(any(proxy::proxy_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit,
        ))
        .with_state(state)
}

async fn handle_health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{call, test_state, test_state_with};
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};

    #[tokio::test]
    async fn health_is_unauthenticated() {
        let state = test_state(&[]).await;
        let resp = call(&state, Request::get("/health").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oversized_admin_bodies_are_refused_before_authentication_work() {
        let state = test_state(&[]).await;
        let huge = vec![b'x'; admin::MAX_ADMIN_BODY + 1];
        let resp = call(
            &state,
            Request::builder()
                .method(Method::POST)
                .uri("/cloud/admin/invite")
                .header("authorization", format!("Bearer {}", state.admin_key))
                .body(Body::from(huge))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn credential_endpoints_are_rate_limited() {
        let state = test_state_with(&[], "auth_rate_limit_per_minute: 3").await;
        let post_enroll = || {
            Request::builder()
                .method(Method::POST)
                .uri("/cloud/enroll")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"code":"AAA-AAA"}"#))
                .unwrap()
        };
        for _ in 0..3 {
            assert_eq!(
                call(&state, post_enroll()).await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let limited = call(&state, post_enroll()).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key("retry-after"));

        // Buckets are per endpoint, and unrelated routes are unaffected.
        let token_req = Request::builder()
            .method(Method::POST)
            .uri("/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("grant_type=nope"))
            .unwrap();
        assert_eq!(
            call(&state, token_req).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&state, Request::get("/health").body(Body::empty()).unwrap())
                .await
                .status(),
            StatusCode::OK
        );
    }
}
