//! Shared helpers for the gateway's tests: a fully wired `GatewayState`, the
//! real router driven in-process, and loopback fake backends.

use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request};
use axum::response::Response;
use axum::Router;
use tower::ServiceExt;

/// The backend token every test gateway forwards (obviously fake).
pub const BACKEND_TOKEN: &str = "test-backend-token-0123456789abcdef-not-a-secret";

use crate::app::build_router;
use crate::auth::SigningKeys;
use crate::config::GatewayConfig;
use crate::revocation::Revocations;
use crate::state::GatewayState;

/// A wired `GatewayState` plus the temp directory holding its secret files;
/// the directory is removed when the last clone is dropped. Derefs to
/// `Arc<GatewayState>`.
#[derive(Clone)]
pub struct TestState {
    state: Arc<GatewayState>,
    _dir: Arc<tempfile::TempDir>,
}

impl Deref for TestState {
    type Target = Arc<GatewayState>;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

/// A state routed at the given `(service, backend_url)` pairs.
pub async fn test_state(routes: &[(&str, &str)]) -> TestState {
    test_state_with(routes, "").await
}

/// Like [`test_state`], with extra `cloud.gateway` YAML lines (e.g.
/// `"max_concurrent_proxy_requests: 1"`).
pub async fn test_state_with(routes: &[(&str, &str)], extra: &str) -> TestState {
    let dir = tempfile::tempdir().unwrap();

    let mut yaml = format!(
        "cloud:\n  gateway:\n    listen: 127.0.0.1:0\n    secret_path: {}/cloud-secret.key\n",
        dir.path().display()
    );
    for line in extra.lines() {
        yaml.push_str(&format!("    {line}\n"));
    }
    yaml.push_str("    routes:\n");
    if routes.is_empty() {
        yaml.push_str("      mcp: http://127.0.0.1:9\n");
    }
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for (service, url) in routes {
        match grouped.iter_mut().find(|(s, _)| s == service) {
            Some((_, urls)) => urls.push(url),
            None => grouped.push((service, vec![url])),
        }
    }
    for (service, urls) in grouped {
        yaml.push_str(&format!("      {service}: [{}]\n", urls.join(", ")));
    }

    let config = GatewayConfig::from_yaml(&yaml, std::path::Path::new("test-config.yaml")).unwrap();
    let keys = SigningKeys::load(&config).unwrap();
    let admin_key =
        hs_common::auth::token::load_or_create_admin_key(&config.admin_key_path()).unwrap();
    let revocations = Revocations::load(config.revocation_path()).unwrap();
    let state = GatewayState::new(
        config,
        keys,
        admin_key,
        revocations,
        "https://gateway.example.com".into(),
        hs_common::auth::backend::BackendToken::new(BACKEND_TOKEN).unwrap(),
    )
    .unwrap();

    TestState {
        state: Arc::new(state),
        _dir: Arc::new(dir),
    }
}

/// Drive one request through the real router.
pub async fn call(state: &Arc<GatewayState>, req: Request<Body>) -> Response {
    build_router(state.clone()).oneshot(req).await.unwrap()
}

/// Like [`call`], with a `ConnectInfo` peer address attached the way
/// `into_make_service_with_connect_info` would — to show the peer address is
/// irrelevant to every decision.
pub async fn call_from(state: &Arc<GatewayState>, mut req: Request<Body>, peer: &str) -> Response {
    let peer: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    call(state, req).await
}

pub fn bearer_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

pub fn json_request(method: Method, path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub async fn body_json(resp: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!("not JSON ({e}): {}", String::from_utf8_lossy(&bytes));
    })
}

/// Serve `router` on an ephemeral loopback port; returns its base URL.
pub async fn spawn_backend(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    format!("http://{addr}")
}

/// A loopback URL nothing is listening on.
pub async fn unused_local_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
