//! The HTTP transport: the streamable-HTTP MCP service behind a shared
//! bearer-token check, with finite idle sessions.
//!
//! Stdio mode never builds this. The gateway reaches this server over the
//! network, so the listener cannot be loopback-only; instead every request
//! must carry `Authorization: Bearer <HS_BACKEND_TOKEN>`
//! (`hs_common::auth::backend`), and the server refuses to start without the
//! secret. There is no unauthenticated health route: nothing is exempt.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use hs_common::auth::backend::{BackendAuthError, BackendToken};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::HomeStillMcp;

/// How long a session may sit without any message before the server drops it.
///
/// Sessions are released by the client's `DELETE`; this is the backstop for
/// clients that vanish. It must outlast the longest tool call that sends no
/// progress notification (a batch `distill_backfill`, a large
/// `personal_add`), because the clock is reset only by session traffic and a
/// call still running when it fires is cancelled with its session.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Build the HTTP app for `server`. Every request, whatever its path or
/// method, is checked against `token` before the MCP service sees it.
pub fn build(server: HomeStillMcp, token: BackendToken, session_idle_timeout: Duration) -> Router {
    build_with_sessions(server, token, session_idle_timeout).0
}

/// [`build`], also returning the session table (tests count sessions in it).
fn build_with_sessions(
    server: HomeStillMcp,
    token: BackendToken,
    session_idle_timeout: Duration,
) -> (Router, Arc<LocalSessionManager>) {
    // Stateful sessions stay: tools stream `notifications/progress` to the
    // caller (scribe_convert), which needs a session to carry them.
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = Some(session_idle_timeout);
    let sessions = Arc::new(session_manager);

    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        sessions.clone(),
        StreamableHttpServerConfig::default(),
    );
    let router = Router::new()
        .fallback_service(service)
        .layer(middleware::from_fn_with_state(token, require_token));
    (router, sessions)
}

async fn require_token(
    State(token): State<BackendToken>,
    request: Request,
    next: Next,
) -> Response {
    match token.check_authorization(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(e) => unauthorized(e),
    }
}

/// 401 with a JSON-RPC error body, so an MCP client that parses the body
/// reports the reason instead of a transport failure.
fn unauthorized(why: BackendAuthError) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32001, "message": format!("unauthorized: {why}") },
    });
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer realm=\"hs-mcp\""),
            (header::CONTENT_TYPE, "application/json"),
        ],
        body.to_string(),
    )
        .into_response()
}

/// Serve `router` on `addr` until ctrl-c.
pub async fn serve(addr: &str, router: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("MCP server listening on {addr}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{server, FaultyStorage};

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    struct Running {
        base: String,
        sessions: Arc<LocalSessionManager>,
        http: reqwest::Client,
    }

    async fn start(idle: Duration) -> Running {
        let (router, sessions) = build_with_sessions(
            server(FaultyStorage::new(), None),
            BackendToken::new(SECRET).unwrap(),
            idle,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Running {
            base,
            sessions,
            http: reqwest::Client::new(),
        }
    }

    fn initialize() -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        })
    }

    fn post(run: &Running, authorization: Option<&str>) -> reqwest::RequestBuilder {
        let mut req = run
            .http
            .post(format!("{}/mcp", run.base))
            .header("Accept", "application/json, text/event-stream")
            .json(&initialize());
        if let Some(value) = authorization {
            req = req.header("Authorization", value);
        }
        req
    }

    /// RA-24: the listener is reachable from the network, so the token is the
    /// only thing between a stranger and `paper_download`/`scribe_convert`.
    #[tokio::test]
    async fn requests_without_a_valid_token_are_401_and_create_no_session() {
        let run = start(Duration::from_secs(60)).await;
        let short = "Bearer short".to_string();
        let wrong = format!("Bearer {}", "x".repeat(SECRET.len()));
        let basic = format!("Basic {SECRET}");
        let cases: [(Option<&str>, &str); 5] = [
            (None, "no header"),
            (Some(&short), "short token"),
            (Some(&wrong), "wrong token"),
            (Some(&basic), "other scheme"),
            (Some("Bearer"), "empty token"),
        ];
        for (header, what) in cases {
            let resp = post(&run, header).send().await.unwrap();
            assert_eq!(resp.status(), 401, "{what}");
            assert_eq!(
                resp.headers()["www-authenticate"].to_str().unwrap(),
                "Bearer realm=\"hs-mcp\""
            );
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["jsonrpc"], "2.0", "{what}");
            assert_eq!(body["error"]["code"], -32001, "{what}");
            assert!(
                !body.to_string().contains(SECRET),
                "{what}: the body must not echo the secret"
            );
        }
        assert_eq!(run.sessions.sessions.read().await.len(), 0);
    }

    #[tokio::test]
    async fn every_method_and_path_is_checked_not_only_the_mcp_endpoint() {
        let run = start(Duration::from_secs(60)).await;
        for (method, path) in [
            (reqwest::Method::GET, "/"),
            (reqwest::Method::GET, "/mcp"),
            (reqwest::Method::DELETE, "/mcp"),
            (reqwest::Method::OPTIONS, "/mcp"),
            (reqwest::Method::POST, "/anything/else"),
        ] {
            let resp = run
                .http
                .request(method.clone(), format!("{}{path}", run.base))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn a_valid_token_reaches_the_mcp_service() {
        let run = start(Duration::from_secs(60)).await;
        let resp = post(&run, Some(&format!("Bearer {SECRET}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let session = resp.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        let body = resp.text().await.unwrap();
        assert!(body.contains("\"protocolVersion\""), "{body}");
        assert_eq!(run.sessions.sessions.read().await.len(), 1);

        // The session id is not a credential: it is useless without the token.
        let resp = run
            .http
            .delete(format!("{}/mcp", run.base))
            .header("mcp-session-id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(run.sessions.sessions.read().await.len(), 1);

        let resp = run
            .http
            .delete(format!("{}/mcp", run.base))
            .header("mcp-session-id", &session)
            .header("Authorization", format!("Bearer {SECRET}"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        assert_eq!(run.sessions.sessions.read().await.len(), 0);
    }

    /// RA-25: clients that initialize and walk away must not pin a session
    /// for the life of the process.
    #[tokio::test]
    async fn idle_sessions_are_dropped() {
        let run = start(Duration::from_millis(300)).await;
        for _ in 0..5 {
            let resp = post(&run, Some(&format!("Bearer {SECRET}")))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
        assert_eq!(run.sessions.sessions.read().await.len(), 5);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !run.sessions.sessions.read().await.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} idle sessions were never released",
                run.sessions.sessions.read().await.len()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Open an authenticated MCP session and return its id.
    async fn open_session(run: &Running) -> String {
        let resp = post(run, Some(&format!("Bearer {SECRET}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let session = resp.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        let _ = resp.text().await.unwrap();
        let resp = run
            .http
            .post(format!("{}/mcp", run.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {SECRET}"))
            .header("mcp-session-id", &session)
            .json(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        session
    }

    /// One JSON-RPC request on `session`; the reply is the SSE frame that
    /// carries the request's id.
    async fn rpc(run: &Running, session: &str, request: serde_json::Value) -> serde_json::Value {
        let resp = run
            .http
            .post(format!("{}/mcp", run.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {SECRET}"))
            .header("mcp-session-id", session)
            .json(&request)
            .send()
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
            .find(|v| v.get("id") == request.get("id"))
            .unwrap_or_else(|| panic!("no reply to {request} in {text}"))
    }

    fn call(id: u64, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        })
    }

    /// RA-4 / RA-27: a stem is untrusted client text that ends up inside
    /// storage keys. Every stem-taking tool refuses a stem that could leave
    /// its prefix, with JSON-RPC invalid-params, before any handler runs.
    #[tokio::test]
    async fn every_tool_that_takes_a_stem_rejects_a_path_like_stem() {
        let run = start(Duration::from_secs(60)).await;
        let session = open_session(&run).await;
        let cases = [
            ("catalog_read", "stem"),
            ("markdown_read", "stem"),
            ("scribe_convert", "stem"),
            ("distill_index", "stem"),
            ("distill_reindex", "stem"),
            ("distill_exists", "doc_id"),
            ("personal_read", "stem"),
            ("personal_reindex", "stem"),
        ];
        let mut id = 10;
        for (tool, field) in cases {
            for bad in ["..", "../../etc/passwd", "a/b", "a\\b", "", "."] {
                id += 1;
                let reply = rpc(
                    &run,
                    &session,
                    call(id, tool, serde_json::json!({ field: bad })),
                )
                .await;
                assert_eq!(
                    reply["error"]["code"], -32602,
                    "{tool}({field}={bad:?}) must be invalid params, got {reply}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_good_stem_passes_the_boundary() {
        let run = start(Duration::from_secs(60)).await;
        let session = open_session(&run).await;
        let reply = rpc(
            &run,
            &session,
            call(2, "catalog_read", serde_json::json!({"stem": "Año"})),
        )
        .await;
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(
            reply["result"]["isError"], true,
            "no such entry is a tool error"
        );
    }

    #[tokio::test]
    async fn resource_uris_with_path_like_stems_are_invalid_params() {
        let run = start(Duration::from_secs(60)).await;
        let session = open_session(&run).await;
        let mut id = 20;
        for uri in [
            "catalog:///..",
            "catalog:///a/b",
            "markdown:///..",
            "markdown:///../x",
            "markdown:///a/b/page/1",
            "markdown:///",
        ] {
            id += 1;
            let reply = rpc(
                &run,
                &session,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": "resources/read",
                    "params": {"uri": uri}
                }),
            )
            .await;
            assert_eq!(reply["error"]["code"], -32602, "{uri}: {reply}");
        }
    }
}
