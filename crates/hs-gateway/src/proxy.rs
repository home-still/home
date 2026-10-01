//! Reverse proxy — forward authenticated requests to LAN backend services.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::{Body, HttpBody};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hs_common::auth::token::TokenType;
use percent_encoding::percent_decode_str;
use tokio::sync::OwnedSemaphorePermit;

use crate::auth;
use crate::state::GatewayState;

/// Generic proxy handler for service routes.
///
/// Every request — LAN or cloud — must present a valid access token. Once the
/// token validates, the handler checks the token's scope against the resolved
/// service and forwards to the backend. ONE PATH: there is no source-IP trust
/// branch.
pub async fn proxy_handler(State(state): State<Arc<GatewayState>>, req: Request<Body>) -> Response {
    let claims = match auth::authenticate(&state, req.headers(), TokenType::Access) {
        Ok(c) => c,
        Err(_) => return unauthorized_response(&state.gateway_url),
    };

    // Determine which service this request is for based on the path
    let service = match resolve_service(req.uri().path()) {
        Ok(s) => s,
        Err(RouteError::BadPath) => {
            return (StatusCode::BAD_REQUEST, "Malformed request path").into_response();
        }
        Err(RouteError::NoSuchService) => {
            return (StatusCode::NOT_FOUND, "No such service").into_response();
        }
    };

    // Check scope
    if !claims.has_scope(service) {
        return (
            StatusCode::FORBIDDEN,
            format!("Token lacks scope: {service}"),
        )
            .into_response();
    }

    // Resolve backend: round-robin over the configured route(s) for the service.
    let Some(pick) = state.balancer.pick(service) else {
        return (
            StatusCode::BAD_GATEWAY,
            format!("No backend configured for service: {service}"),
        )
            .into_response();
    };
    let backend_base = pick.url.clone();

    let permit = match state.proxy_permits.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let mut resp = (StatusCode::SERVICE_UNAVAILABLE, "Gateway is busy").into_response();
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            return resp;
        }
    };

    // Path AND query: the query string is part of the request.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| req.uri().path());
    let backend_url = format!("{backend_base}{path_and_query}");

    forward_request(&state, service, pick.index, req, backend_url, permit).await
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteError {
    /// The path is not one we will forward (dot segments, encoded separators…).
    BadPath,
    /// No service owns this path.
    NoSuchService,
}

/// Reject any path whose meaning could change between here and the backend:
/// dot segments (raw or percent-encoded), empty segments, encoded or raw
/// separators, NUL and control characters.
fn check_path(path: &str) -> Result<(), RouteError> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(RouteError::BadPath);
    };
    if path.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\') {
        return Err(RouteError::BadPath);
    }
    let last = rest.split('/').count() - 1;
    for (i, segment) in rest.split('/').enumerate() {
        // `//` is never meaningful; only a single trailing `/` yields an
        // empty final segment.
        if segment.is_empty() && i != last {
            return Err(RouteError::BadPath);
        }
        let decoded: Vec<u8> = percent_decode_str(segment).collect();
        if decoded == b"." || decoded == b".." {
            return Err(RouteError::BadPath);
        }
        if decoded.iter().any(|&b| matches!(b, b'/' | b'\\' | 0)) {
            return Err(RouteError::BadPath);
        }
    }
    Ok(())
}

/// Map a gateway path to the service that owns it, by path-segment equality:
///
/// * `/mcp[/…]`, `/scribe[/…]`, `/distill[/…]` → that service
/// * `/search` and `/exists/<id>` → distill
///
/// `/mcpfoo` is not `/mcp`. The path is forwarded to the backend unchanged.
pub fn resolve_service(path: &str) -> Result<&'static str, RouteError> {
    check_path(path)?;
    let mut segments = path[1..].split('/');
    let first = segments.next().unwrap_or_default();
    let second = segments.next();
    match first {
        "mcp" => Ok("mcp"),
        "scribe" => Ok("scribe"),
        "distill" => Ok("distill"),
        "search" if second.is_none() => Ok("distill"),
        "exists" if second.is_some_and(|s| !s.is_empty()) => Ok("distill"),
        _ => Err(RouteError::NoSuchService),
    }
}

/// Headers that describe one connection, not the message (RFC 9110 §7.6.1).
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Request headers that must not reach a backend: the gateway's own
/// credentials, `Host` (rewritten for the backend), and everything a client or
/// CDN may have set to describe the original caller — a backend must not
/// believe a spoofed `X-Forwarded-For`.
fn is_client_only(name: &str) -> bool {
    name == "host"
        || name == "authorization"
        || name.starts_with("cf-")
        || name.starts_with("x-forwarded-")
        || matches!(
            name,
            "forwarded" | "x-real-ip" | "true-client-ip" | "cdn-loop"
        )
}

/// Copy `src`, dropping hop-by-hop headers, headers named in `Connection`, and
/// anything `extra_drop` rejects.
fn filtered_headers(src: &HeaderMap, extra_drop: impl Fn(&str) -> bool) -> HeaderMap {
    let named_in_connection: Vec<String> = src
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        if is_hop_by_hop(n) || named_in_connection.iter().any(|c| c == n) || extra_drop(n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[derive(Debug)]
struct BodyTooLarge;

impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "request body exceeds the configured limit")
    }
}

impl std::error::Error for BodyTooLarge {}

type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// Forward an HTTP request to a backend URL, streaming both directions.
///
/// The request body is passed through as a stream (never buffered whole) with
/// a hard size cap; the response body is streamed back and keeps `permit`
/// until the client has it all or goes away.
async fn forward_request(
    state: &GatewayState,
    service: &str,
    index: usize,
    original: Request<Body>,
    backend_url: String,
    permit: OwnedSemaphorePermit,
) -> Response {
    let max_body = state.config.max_request_body_bytes;
    let (parts, body) = original.into_parts();

    let declared_len = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_len.is_some_and(|len| len > max_body) {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
    }
    // Whether there is a body is the body's own answer, not a header's: HTTP/2
    // bodies carry neither Content-Length nor Transfer-Encoding.
    let has_body = !body.is_end_stream();

    // The caller's Authorization (a gateway-signed token) is dropped by
    // `is_client_only`; the backend gets the shared backend secret instead.
    // `bearer_auth` marks the value sensitive so it is redacted from Debug.
    let mut backend_req = state
        .http
        .request(parts.method, &backend_url)
        .headers(filtered_headers(&parts.headers, is_client_only))
        .bearer_auth(state.backend_token.expose_secret());

    let too_large = Arc::new(AtomicBool::new(false));
    if has_body {
        let flag = Arc::clone(&too_large);
        let mut seen: u64 = 0;
        let stream = body.into_data_stream().map(move |chunk| match chunk {
            Ok(bytes) => {
                seen += bytes.len() as u64;
                if seen > max_body {
                    flag.store(true, Ordering::Relaxed);
                    Err(Box::new(BodyTooLarge) as BodyError)
                } else {
                    Ok(bytes)
                }
            }
            Err(e) => Err(Box::new(e) as BodyError),
        });
        backend_req = backend_req.body(reqwest::Body::wrap_stream(stream));
    }

    let backend_resp = match backend_req.send().await {
        Ok(r) => r,
        Err(e) => {
            if too_large.load(Ordering::Relaxed) {
                return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
            }
            if e.is_connect() {
                state.balancer.mark_failed(service, index);
            }
            // The backend address is internal; log it, don't hand it to the client.
            tracing::error!("backend request to {backend_url} failed: {e}");
            return if e.is_timeout() {
                (StatusCode::GATEWAY_TIMEOUT, "Backend timed out").into_response()
            } else {
                (StatusCode::BAD_GATEWAY, "Backend unreachable").into_response()
            };
        }
    };

    let status = backend_resp.status();
    let headers = filtered_headers(backend_resp.headers(), |_| false);
    let stream = backend_resp.bytes_stream().map(move |chunk| {
        let _held = &permit;
        chunk
    });

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
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
    use crate::testutil::{
        body_json, call, spawn_backend, test_state, test_state_with, unused_local_url,
    };
    use axum::http::Method;
    use axum::routing::{any, get, post};
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use std::time::Duration;

    #[test]
    fn service_resolution_is_by_path_segment() {
        for (path, want) in [
            ("/scribe", "scribe"),
            ("/scribe/", "scribe"),
            ("/scribe/stream", "scribe"),
            ("/distill/stream", "distill"),
            ("/distill", "distill"),
            ("/search", "distill"),
            ("/exists/abc123", "distill"),
            ("/mcp", "mcp"),
            ("/mcp/", "mcp"),
            ("/mcp/sse", "mcp"),
            ("/scribe/a%20b", "scribe"),
        ] {
            assert_eq!(resolve_service(path), Ok(want), "{path}");
        }
    }

    #[test]
    fn a_prefix_match_is_not_a_service_match() {
        for path in [
            "/mcpfoo",
            "/mcp-evil/x",
            "/scribefoo",
            "/scribes/x",
            "/distillery",
            "/distill2/x",
            "/search/x",
            "/search/",
            "/exists",
            "/exists/",
            "/existsx/abc",
            "/",
            "/readiness",
            "/status",
            "/other/path",
        ] {
            assert_eq!(
                resolve_service(path),
                Err(RouteError::NoSuchService),
                "{path}"
            );
        }
    }

    #[test]
    fn paths_that_could_change_meaning_downstream_are_rejected() {
        for path in [
            "/scribe/../admin",
            "/scribe/./x",
            "/scribe/..",
            "/scribe/%2e%2e/x",
            "/scribe/%2E%2E/x",
            "/scribe/.%2e/x",
            "/scribe/%2e/x",
            "/scribe/%2fx",
            "/scribe/a%2Fb",
            "/scribe/%5cx",
            "/scribe/x\\y",
            "/scribe/%00",
            "/scribe//x",
            "//scribe/x",
            "/mcp/%2e%2e/%2e%2e/etc/passwd",
            "scribe/x",
        ] {
            assert_eq!(resolve_service(path), Err(RouteError::BadPath), "{path}");
        }
    }

    // ── proxy behavior against a local fake backend ────────────

    fn bearer_req(method: Method, uri: &str, token: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
    }

    fn access(state: &Arc<GatewayState>, scopes: &[&str]) -> String {
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        auth::issue_token(state, "laptop", &scopes, TokenType::Access).unwrap()
    }

    /// A backend that reports what it received.
    fn echo_router() -> Router {
        async fn echo(req: Request<Body>) -> Json<Value> {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let headers: serde_json::Map<String, Value> = parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), json!(v.to_str().unwrap_or("<bin>"))))
                .collect();
            Json(json!({
                "method": parts.method.as_str(),
                "uri": parts.uri.to_string(),
                "headers": headers,
                "body_len": bytes.len(),
            }))
        }
        Router::new().fallback(any(echo))
    }

    #[tokio::test]
    async fn path_and_query_reach_the_backend_unchanged() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state(&[("distill", &backend)]).await;
        let token = access(&state, &["distill"]);

        let resp = call(
            &state,
            bearer_req(
                Method::GET,
                "/exists/doc-1?limit=5&q=two%20words&flag",
                &token,
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let seen = body_json(resp).await;
        assert_eq!(seen["uri"], "/exists/doc-1?limit=5&q=two%20words&flag");
        assert_eq!(seen["method"], "GET");
    }

    #[tokio::test]
    async fn refresh_tokens_and_unscoped_tokens_never_reach_a_backend() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state(&[("scribe", &backend), ("mcp", &backend)]).await;

        let refresh = auth::issue_token(
            &state,
            "laptop",
            &["scribe".to_string()],
            TokenType::Refresh,
        )
        .unwrap();
        let resp = call(
            &state,
            bearer_req(Method::GET, "/scribe/x", &refresh)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key("www-authenticate"));

        let scribe_only = access(&state, &["scribe"]);
        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/x", &scribe_only)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call(
            &state,
            Request::get("/scribe/x").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hostile_paths_are_refused_without_contacting_the_backend() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state(&[("mcp", &backend), ("scribe", &backend)]).await;
        let token = access(&state, &["mcp", "scribe"]);

        for (uri, want) in [
            ("/scribe/../mcp/x", StatusCode::BAD_REQUEST),
            ("/scribe/%2e%2e/mcp/x", StatusCode::BAD_REQUEST),
            ("/mcpfoo", StatusCode::NOT_FOUND),
            ("/unknown/x", StatusCode::NOT_FOUND),
        ] {
            let resp = call(
                &state,
                bearer_req(Method::GET, uri, &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(resp.status(), want, "{uri}");
        }
    }

    #[tokio::test]
    async fn hop_by_hop_and_client_supplied_forwarding_headers_are_stripped() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state(&[("mcp", &backend)]).await;
        let token = access(&state, &["mcp"]);

        let req = bearer_req(Method::GET, "/mcp/x", &token)
            .header("x-forwarded-for", "203.0.113.9")
            .header("x-forwarded-host", "evil.example")
            .header("forwarded", "for=203.0.113.9")
            .header("x-real-ip", "203.0.113.9")
            .header("cf-connecting-ip", "203.0.113.9")
            .header("cf-access-client-secret", "s3cret")
            .header("connection", "keep-alive, x-private")
            .header("x-private", "1")
            .header("keep-alive", "timeout=5")
            .header("te", "trailers")
            .header("upgrade", "websocket")
            .header("x-app-header", "kept")
            .body(Body::empty())
            .unwrap();
        let seen = body_json(call(&state, req).await).await;
        let headers = seen["headers"].as_object().unwrap();

        for gone in [
            "x-forwarded-for",
            "x-forwarded-host",
            "forwarded",
            "x-real-ip",
            "cf-connecting-ip",
            "cf-access-client-secret",
            "x-private",
            "keep-alive",
            "te",
            "upgrade",
        ] {
            assert!(!headers.contains_key(gone), "{gone} leaked: {headers:?}");
        }
        // The only credential a backend sees is the shared backend token,
        // never the caller's gateway token.
        assert_eq!(headers["authorization"], expected_auth().as_str());
        assert_eq!(headers["x-app-header"], "kept");
    }

    #[tokio::test]
    async fn request_bodies_stream_through_and_arrive_intact() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state(&[("scribe", &backend)]).await;
        let token = access(&state, &["scribe"]);

        // 3 MiB delivered as many chunks with NO Content-Length (chunked).
        let chunk = vec![7u8; 64 * 1024];
        let chunks = 48;
        let stream = futures_util::stream::iter(
            (0..chunks)
                .map(move |_| Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk.clone()))),
        );
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .body(Body::from_stream(stream))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["body_len"], 64 * 1024 * chunks);

        // And with a Content-Length.
        let payload = vec![1u8; 1_000_000];
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .header("content-length", payload.len())
                .body(Body::from(payload))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["body_len"], 1_000_000);
    }

    #[tokio::test]
    async fn oversized_bodies_are_cut_off_with_413() {
        let backend = spawn_backend(echo_router()).await;
        let state = test_state_with(&[("scribe", &backend)], "max_request_body_bytes: 1024").await;
        let token = access(&state, &["scribe"]);

        // Declared up front.
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .header("content-length", 5000)
                .body(Body::from(vec![0u8; 5000]))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Undeclared (chunked): caught while streaming.
        let stream = futures_util::stream::iter(
            (0..10).map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![0u8; 512]))),
        );
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .body(Body::from_stream(stream))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // At the limit is fine.
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .header("content-length", 1024)
                .body(Body::from(vec![0u8; 1024]))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn response_bodies_stream_back_with_the_backend_status() {
        async fn chunks() -> impl IntoResponse {
            let stream =
                futures_util::stream::iter((0..5).map(|i| {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("line {i}\n")))
                }));
            (
                StatusCode::CREATED,
                [("x-backend", "yes")],
                Body::from_stream(stream),
            )
        }
        let backend = spawn_backend(Router::new().route("/scribe/stream", post(chunks))).await;
        let state = test_state(&[("scribe", &backend)]).await;
        let token = access(&state, &["scribe"]);

        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(resp.headers()["x-backend"], "yes");
        let text = crate::testutil::body_string(resp).await;
        assert_eq!(text, "line 0\nline 1\nline 2\nline 3\nline 4\n");
    }

    #[tokio::test]
    async fn backend_redirects_are_passed_through_not_followed() {
        async fn redirect() -> impl IntoResponse {
            (
                StatusCode::FOUND,
                [("location", "http://169.254.169.254/latest")],
                "",
            )
        }
        let backend = spawn_backend(Router::new().route("/mcp/r", get(redirect))).await;
        let state = test_state(&[("mcp", &backend)]).await;
        let token = access(&state, &["mcp"]);
        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/r", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(resp.headers()["location"], "http://169.254.169.254/latest");
    }

    #[tokio::test]
    async fn unreachable_backend_is_502_and_does_not_leak_its_address() {
        let dead = unused_local_url().await;
        let state = test_state(&[("mcp", &dead)]).await;
        let token = access(&state, &["mcp"]);
        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/x", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = crate::testutil::body_string(resp).await;
        assert!(!body.contains("127.0.0.1"), "{body}");
    }

    #[tokio::test]
    async fn a_stalled_backend_times_out_with_504() {
        async fn slow() -> &'static str {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "late"
        }
        let backend = spawn_backend(Router::new().route("/mcp/slow", get(slow))).await;
        let state = test_state_with(&[("mcp", &backend)], "backend_read_timeout_secs: 1").await;
        let token = access(&state, &["mcp"]);
        let started = std::time::Instant::now();
        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/slow", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn concurrency_is_bounded_and_permits_are_released() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let backend = {
            let gate = gate.clone();
            spawn_backend(Router::new().route(
                "/mcp/hold",
                get(move || {
                    let gate = gate.clone();
                    async move {
                        gate.notified().await;
                        "released"
                    }
                }),
            ))
            .await
        };
        let state = test_state_with(&[("mcp", &backend)], "max_concurrent_proxy_requests: 1").await;
        let token = access(&state, &["mcp"]);

        let first = {
            let state = state.clone();
            let token = token.clone();
            tokio::spawn(async move {
                call(
                    &state,
                    bearer_req(Method::GET, "/mcp/hold", &token)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
            })
        };
        // Wait until the first request holds the only permit.
        for _ in 0..200 {
            if state.proxy_permits.available_permits() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.proxy_permits.available_permits(), 0);

        let second = call(
            &state,
            bearer_req(Method::GET, "/mcp/hold", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);

        gate.notify_one();
        let first = first.await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let _ = crate::testutil::body_string(first).await;
        for _ in 0..200 {
            if state.proxy_permits.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.proxy_permits.available_permits(), 1);
    }

    /// A backend that answers with its own name.
    fn named_backend(name: &'static str) -> Router {
        Router::new().fallback(any(move || async move { name }))
    }

    #[tokio::test]
    async fn a_list_route_round_robins_across_its_instances() {
        let a = spawn_backend(named_backend("a")).await;
        let b = spawn_backend(named_backend("b")).await;
        let state = test_state(&[("scribe", &a), ("scribe", &b)]).await;
        let token = access(&state, &["scribe"]);

        let mut seen = Vec::new();
        for _ in 0..6 {
            let resp = call(
                &state,
                bearer_req(Method::GET, "/scribe/x", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
            seen.push(crate::testutil::body_string(resp).await);
        }
        assert_eq!(seen.iter().filter(|s| *s == "a").count(), 3, "{seen:?}");
        assert_eq!(seen.iter().filter(|s| *s == "b").count(), 3, "{seen:?}");
    }

    #[tokio::test]
    async fn an_instance_that_refuses_connections_is_skipped_for_the_cooldown() {
        let dead = unused_local_url().await;
        let live = spawn_backend(named_backend("live")).await;
        let state = test_state_with(
            &[("scribe", &dead), ("scribe", &live)],
            "backend_failure_cooldown_secs: 60",
        )
        .await;
        let token = access(&state, &["scribe"]);
        let get = || {
            bearer_req(Method::GET, "/scribe/x", &token)
                .body(Body::empty())
                .unwrap()
        };

        // Within the first two requests the dead instance is tried once: 502.
        let mut statuses = Vec::new();
        for _ in 0..2 {
            statuses.push(call(&state, get()).await.status());
        }
        assert!(statuses.contains(&StatusCode::BAD_GATEWAY), "{statuses:?}");

        // From then on every request goes to the live instance.
        for _ in 0..6 {
            let resp = call(&state, get()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(crate::testutil::body_string(resp).await, "live");
        }
    }

    async fn authorizations(resp: Response) -> Vec<String> {
        let text = crate::testutil::body_string(resp).await;
        text.split('|')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn recording_backend() -> Router {
        async fn record(req: Request<Body>) -> String {
            let seen: Vec<String> = req
                .headers()
                .get_all(header::AUTHORIZATION)
                .iter()
                .map(|v| v.to_str().unwrap_or("<bin>").to_string())
                .collect();
            let _ = axum::body::to_bytes(req.into_body(), usize::MAX).await;
            seen.join("|")
        }
        Router::new().fallback(any(record))
    }

    fn expected_auth() -> String {
        format!("Bearer {}", crate::testutil::BACKEND_TOKEN)
    }

    #[tokio::test]
    async fn backends_get_exactly_one_authorization_header_the_backend_token() {
        let a = spawn_backend(recording_backend()).await;
        let b = spawn_backend(recording_backend()).await;
        let state = test_state(&[("scribe", &a), ("scribe", &b)]).await;
        let user_token = access(&state, &["scribe"]);

        // GET, and round-robin across both list instances.
        for _ in 0..4 {
            let resp = call(
                &state,
                bearer_req(Method::GET, "/scribe/x", &user_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(authorizations(resp).await, [expected_auth()]);
        }

        // Streamed (chunked) POST.
        let stream = futures_util::stream::iter(
            (0..8).map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![1u8; 4096]))),
        );
        let resp = call(
            &state,
            bearer_req(Method::POST, "/scribe/stream", &user_token)
                .body(Body::from_stream(stream))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(authorizations(resp).await, [expected_auth()]);
    }

    #[tokio::test]
    async fn the_users_gateway_token_is_never_forwarded_even_when_sent_twice() {
        let backend = spawn_backend(recording_backend()).await;
        let state = test_state(&[("mcp", &backend)]).await;
        let user_token = access(&state, &["mcp"]);

        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/x", &user_token)
                .header("authorization", "Bearer client-supplied-extra")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let seen = authorizations(resp).await;
        assert_eq!(seen, [expected_auth()]);
        assert!(!seen.iter().any(|v| v.contains(&user_token)));
    }

    #[tokio::test]
    async fn the_backend_token_never_reaches_the_client() {
        // A refused/failed upstream must not echo the secret in error bodies.
        let dead = unused_local_url().await;
        let state = test_state(&[("mcp", &dead)]).await;
        let user_token = access(&state, &["mcp"]);
        let resp = call(
            &state,
            bearer_req(Method::GET, "/mcp/x", &user_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body = crate::testutil::body_string(resp).await;
        assert!(!body.contains(crate::testutil::BACKEND_TOKEN));
        assert!(!format!("{:?}", state.backend_token).contains(crate::testutil::BACKEND_TOKEN));
    }
}
