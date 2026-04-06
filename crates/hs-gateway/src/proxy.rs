//! Reverse proxy — forward authenticated requests to LAN backend services.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};

use hs_common::auth::token::{self, TokenError};

use crate::state::GatewayState;

/// Generic proxy handler for service routes.
///
/// Validates the bearer token, checks scope, then forwards the request
/// to the appropriate backend service.
pub async fn proxy_handler(State(state): State<Arc<GatewayState>>, req: Request<Body>) -> Response {
    // Extract bearer token
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token_str = match auth_header {
        Some(t) => t.to_string(),
        None => {
            return unauthorized_response(&state.gateway_url);
        }
    };

    // Validate token
    let claims = match token::validate_token(&state.secret, &token_str, false) {
        Ok(c) => c,
        Err(TokenError::Expired) => {
            return unauthorized_response(&state.gateway_url);
        }
        Err(_) => {
            return unauthorized_response(&state.gateway_url);
        }
    };

    // Determine which service this request is for based on the path
    let path = req.uri().path();
    let (service_name, backend_path) = resolve_service(path);

    // Check scope
    if !claims.has_scope(&service_name) {
        return (
            StatusCode::FORBIDDEN,
            format!("Token lacks scope: {service_name}"),
        )
            .into_response();
    }

    // Resolve backend URL
    let backend_base = match state.config.backend_for(&service_name) {
        Some(url) => url.to_string(),
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("No backend configured for service: {service_name}"),
            )
                .into_response();
        }
    };

    let backend_url = format!("{backend_base}{backend_path}");

    // Forward the request
    forward_request(&state.http, req, &backend_url).await
}

/// Map a gateway path to (service_name, backend_path).
fn resolve_service(path: &str) -> (String, String) {
    // /scribe/stream -> service="scribe", backend_path="/scribe/stream"
    // /distill/stream -> service="distill", backend_path="/distill/stream"
    // /search -> service="distill", backend_path="/search"
    // /health -> service="health", backend_path="/health" (handled separately)

    if path.starts_with("/mcp") {
        ("mcp".into(), path.into())
    } else if path.starts_with("/scribe") {
        ("scribe".into(), path.into())
    } else if path.starts_with("/distill") || path == "/search" || path.starts_with("/exists/") {
        ("distill".into(), path.into())
    } else {
        // Default: use the first path segment as service name
        let service = path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or("unknown");
        (service.into(), path.into())
    }
}

/// Forward an HTTP request to a backend URL, streaming the response back.
async fn forward_request(
    http: &reqwest::Client,
    original: Request<Body>,
    backend_url: &str,
) -> Response {
    let method = original.method().clone();
    let headers = original.headers().clone();

    // Build the backend request
    let mut backend_req = http.request(method, backend_url);

    // Forward relevant headers (skip auth — backend doesn't need it)
    for (name, value) in &headers {
        if name == axum::http::header::AUTHORIZATION
            || name == axum::http::header::HOST
            || name == "cf-access-client-id"
            || name == "cf-access-client-secret"
            || name == "cf-access-jwt-assertion"
        {
            continue;
        }
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
            backend_req = backend_req.header(name.as_str(), v);
        }
    }

    // Forward the body
    let body_bytes = match axum::body::to_bytes(original.into_body(), 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to read request body: {e}"),
            )
                .into_response();
        }
    };
    backend_req = backend_req.body(body_bytes);

    // Send to backend
    let backend_resp = match backend_req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Backend request failed: {e}");
            return (StatusCode::BAD_GATEWAY, format!("Backend unreachable: {e}")).into_response();
        }
    };

    // Convert backend response to axum response, streaming the body
    let status = StatusCode::from_u16(backend_resp.status().as_u16()).unwrap_or(StatusCode::OK);
    let resp_headers = backend_resp.headers().clone();

    let stream = backend_resp.bytes_stream();
    let body = Body::from_stream(stream);

    let mut response = Response::builder().status(status);
    for (name, value) in &resp_headers {
        response = response.header(name, value);
    }

    response.body(body).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "Response build failed").into_response()
    })
}

/// Build a 401 response with WWW-Authenticate header for OAuth discovery.
fn unauthorized_response(gateway_url: &str) -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(
            "WWW-Authenticate",
            format!(
                r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#,
                gateway_url
            ),
        )
        .body(Body::from("Unauthorized"))
        .unwrap_or_else(|_| (StatusCode::UNAUTHORIZED, "Unauthorized").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_resolution() {
        assert_eq!(
            resolve_service("/scribe/stream"),
            ("scribe".into(), "/scribe/stream".into())
        );
        assert_eq!(
            resolve_service("/distill/stream"),
            ("distill".into(), "/distill/stream".into())
        );
        assert_eq!(
            resolve_service("/search"),
            ("distill".into(), "/search".into())
        );
        assert_eq!(
            resolve_service("/exists/abc123"),
            ("distill".into(), "/exists/abc123".into())
        );
    }
}
