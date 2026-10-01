//! The gateway's HTTP router.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{any, get, post};
use axum::Router;

use crate::state::GatewayState;
use crate::{admin, enrollment, oauth, proxy, ratelimit};

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
        // Authenticated proxy — catch all remaining paths
        .fallback(any(proxy::proxy_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit,
        ))
        .with_state(state)
        // Outermost: a handler panic is a 500 and the server keeps serving.
        .layer(middleware::from_fn(
            hs_common::panic_guard::http::catch_panic,
        ))
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

    fn token_post(body: String) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn an_anonymous_flood_of_bad_grants_cannot_lock_out_a_valid_refresh() {
        use hs_common::auth::token::TokenType;
        let state = test_state_with(&[], "auth_rate_limit_per_minute: 3").await;
        let refresh = crate::auth::issue_token(
            &state,
            "oauth:hs-client",
            &["mcp".to_string()],
            TokenType::Refresh,
        )
        .unwrap();
        let valid = || token_post(format!("grant_type=refresh_token&refresh_token={refresh}"));
        let bad = || token_post("grant_type=refresh_token&refresh_token=forged".into());

        // The attacker burns the whole failure budget, then gets 429.
        for _ in 0..3 {
            assert_eq!(call(&state, bad()).await.status(), StatusCode::UNAUTHORIZED);
        }
        let limited = call(&state, bad()).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key("retry-after"));

        // A valid grant is still served, repeatedly, with the budget spent.
        for _ in 0..5 {
            assert_eq!(call(&state, valid()).await.status(), StatusCode::OK);
        }
        // And a wrong-class token is a failure, not a free pass.
        let access = crate::auth::issue_token(
            &state,
            "oauth:hs-client",
            &["mcp".to_string()],
            TokenType::Access,
        )
        .unwrap();
        let wrong_class = call(
            &state,
            token_post(format!("grant_type=refresh_token&refresh_token={access}")),
        )
        .await;
        assert_eq!(wrong_class.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn enrollment_code_guessing_stays_bounded_and_valid_enrolments_are_free() {
        let state = test_state_with(&[], "auth_rate_limit_per_minute: 3").await;
        let enroll = |code: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/cloud/enroll")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"code":"{code}"}}"#)))
                .unwrap()
        };

        // Successful enrolments do not use the budget.
        for _ in 0..5 {
            let code = crate::enrollment::register_enrollment(
                &state.enrollments,
                "laptop",
                vec!["mcp".into()],
            )
            .unwrap();
            assert_eq!(call(&state, enroll(&code)).await.status(), StatusCode::OK);
        }

        // Guessing is capped at the budget...
        let mut guesses_answered = 0;
        for _ in 0..10 {
            if call(&state, enroll("AAA-AAA")).await.status() == StatusCode::UNAUTHORIZED {
                guesses_answered += 1;
            }
        }
        assert_eq!(guesses_answered, 3);

        // ...and while exhausted even a real code is refused (a guess cannot
        // be told apart from it before it is looked up).
        let code = crate::enrollment::register_enrollment(
            &state.enrollments,
            "laptop",
            vec!["mcp".into()],
        )
        .unwrap();
        assert_eq!(
            call(&state, enroll(&code)).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
