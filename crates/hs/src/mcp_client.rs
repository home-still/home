//! Tiny HTTP client for the home-still MCP server behind the cloud gateway.
//!
//! Used by `hs status` (and other read-only CLI commands) on client-role nodes
//! where the authoritative data lives on a remote server and this node has no
//! usable local filesystem view. Mirrors what `npx mcp-remote` does for
//! Claude Desktop, but in-process — no Node, no long-polling, just POST and
//! parse the SSE-wrapped JSON result.
//!
//! Wiring:
//!   1. Load cached cloud creds via `AuthenticatedClient::from_default_path()`.
//!   2. Open the MCP session: POST initialize (capture `mcp-session-id`), then
//!      POST `notifications/initialized`. Both must succeed.
//!   3. `call_tool(name, args)` POSTs a `tools/call` request and returns the
//!      tool's text content parsed as JSON (tools always return JSON strings
//!      inside a text content frame, at least in this project's server). A
//!      non-2xx answer, a JSON-RPC error, or a result flagged `isError` is an
//!      `Err`, never data.
//!   4. `close()` ends the session with `DELETE` so the server does not keep
//!      one session per `hs status` invocation.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use hs_common::auth::client::{AuthedHttp, AuthenticatedClient};

pub struct McpClient {
    http: AuthedHttp,
    endpoint: String,
    session_id: Option<String>,
}

/// How much of a response body is quoted in an error message.
const BODY_EXCERPT_CHARS: usize = 500;

fn excerpt(body: &str) -> String {
    body.chars().take(BODY_EXCERPT_CHARS).collect()
}

impl McpClient {
    /// Build a client and complete the MCP handshake. Two endpoint paths:
    ///
    /// - `HS_MCP_URL` env var set: use it verbatim, no auth. For same-host
    ///   and LAN-direct operation where round-tripping through the cloud
    ///   gateway (and depending on a 7-day refresh token) is unnecessary.
    ///   Example: `HS_MCP_URL=http://localhost:7445/mcp`.
    /// - Otherwise: load cached cloud creds and route through the gateway.
    pub async fn from_default_creds() -> Result<Self> {
        if let Ok(direct) = std::env::var("HS_MCP_URL") {
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("build direct-mode http client for MCP")?;
            return Self::connect(AuthedHttp::plain(http), direct).await;
        }

        let auth = AuthenticatedClient::from_default_path()
            .context("load cloud credentials (hs cloud enroll --gateway <url>)")?;
        let endpoint = format!("{}/mcp", auth.gateway_url().trim_end_matches('/'));
        let http = AuthedHttp::with_auth(auth, std::time::Duration::from_secs(30))
            .context("build authenticated http client for MCP")?;
        Self::connect(http, endpoint).await
    }

    /// Open a session against `endpoint` with an already-built client.
    async fn connect(http: AuthedHttp, endpoint: String) -> Result<Self> {
        let mut client = Self {
            http,
            endpoint,
            session_id: None,
        };
        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&mut self) -> Result<()> {
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "hs-cli", "version": env!("CARGO_PKG_VERSION")}
            }
        });
        let resp = self
            .http
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .json(&init)
            .send()
            .await
            .context("POST MCP initialize")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MCP initialize failed ({status}): {}", excerpt(&body));
        }
        self.session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        // Drain body, session is ready once we've read the headers.
        resp.text()
            .await
            .context("read MCP initialize response body")?;

        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        self.post(&notif)
            .await
            .context("MCP notifications/initialized")?;
        Ok(())
    }

    /// POST one JSON-RPC message in the session; returns the response body.
    /// Any non-2xx status is an error carrying the status and the start of
    /// the body.
    async fn post(&self, body: &Value) -> Result<String> {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .json(body);
        if let Some(sid) = &self.session_id {
            req = req.header("mcp-session-id", sid);
        }
        let resp = req.send().await.context("POST MCP body")?;
        let status = resp.status();
        let text = resp.text().await.context("read MCP response body")?;
        if !status.is_success() {
            anyhow::bail!("MCP request failed ({status}): {}", excerpt(&text));
        }
        Ok(text)
    }

    /// Invoke a tool and return its text content parsed as JSON.
    ///
    /// If the tool's text payload isn't valid JSON (some tools return plain
    /// strings), returns it wrapped in `Value::String`. A tool that reports
    /// `isError: true` (or a JSON-RPC error) is an `Err` carrying its message.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let body = self.post(&req).await?;
        parse_tool_result(name, &body)
    }

    /// End the MCP session (`DELETE` with the session id). Without it the
    /// server keeps the session until it expires. A server that issued no
    /// session id has nothing to close.
    pub async fn close(self) -> Result<()> {
        let Some(sid) = &self.session_id else {
            return Ok(());
        };
        let resp = self
            .http
            .delete(&self.endpoint)
            .header("mcp-session-id", sid)
            .send()
            .await
            .context("DELETE MCP session")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MCP session DELETE failed ({status}): {}", excerpt(&body));
        }
        Ok(())
    }

    /// [`close`](Self::close), logging a failure: the data the caller wanted
    /// is already in hand, but a server that keeps sessions open should show
    /// up in the logs rather than vanish.
    pub async fn close_logged(self) {
        if let Err(e) = self.close().await {
            tracing::warn!(error = %e, "could not end the MCP session; the server keeps it until it expires");
        }
    }
}

/// Pull the tool result out of the first SSE `data:` frame that carries a
/// JSON-RPC `result` or `error`:
///
/// - `error` (JSON-RPC level) -> `Err` with its message;
/// - `result.isError == true` -> `Err` with the tool's text;
/// - otherwise `result.content[0].text`, parsed as JSON (a plain string stays
///   a `Value::String`).
fn parse_tool_result(name: &str, body: &str) -> Result<Value> {
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(err) = v.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| err.to_string());
            return Err(anyhow!("MCP error from '{name}': {}", excerpt(&message)));
        }
        let Some(result) = v.get("result") else {
            continue;
        };
        let text = result.pointer("/content/0/text").and_then(Value::as_str);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(anyhow!(
                "tool '{name}' reported an error: {}",
                excerpt(text.unwrap_or("(no message)"))
            ));
        }
        let Some(text) = text else {
            continue;
        };
        return Ok(
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string()))
        );
    }
    Err(anyhow!(
        "no tool result in MCP response for '{name}': body={}",
        excerpt(body)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{FakeServer, Response};

    fn sse(frame: Value) -> Response {
        Response::raw(
            200,
            "text/event-stream",
            &format!("data: \n\nid: 0\nretry: 3000\n\ndata: {frame}\n\n"),
        )
    }

    fn tool_ok(text: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 99, "result": {
            "content": [{"type": "text", "text": text}], "isError": false}})
    }

    /// Answers initialize (with a session id), the initialized notification
    /// and `tools/call`; everything else (DELETE) gets `delete_status`.
    async fn mcp_server(tool_reply: Response, delete_status: u16) -> FakeServer {
        FakeServer::start(move |req| {
            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            match (req.method.as_str(), rpc["method"].as_str()) {
                ("POST", Some("initialize")) => sse(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
                    .with_header("mcp-session-id", "sess-1"),
                ("POST", Some("notifications/initialized")) => Response::raw(202, "text/plain", ""),
                ("POST", Some("tools/call")) => tool_reply.clone(),
                ("DELETE", _) => Response::raw(delete_status, "text/plain", ""),
                _ => Response::raw(404, "text/plain", "no such route"),
            }
        })
        .await
    }

    async fn client(server: &FakeServer) -> Result<McpClient> {
        McpClient::connect(
            AuthedHttp::plain(reqwest::Client::new()),
            format!("{}/mcp", server.base),
        )
        .await
    }

    #[test]
    fn parses_sse_result_json_text() {
        let body = "data: \n\nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"catalog_entries\\\":2855}\"}],\"isError\":false}}\n";
        let parsed = parse_tool_result("system_status", body).unwrap();
        assert_eq!(parsed["catalog_entries"], 2855);
    }

    #[test]
    fn returns_string_for_non_json_text() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"plain string here\"}]}}\n";
        let parsed = parse_tool_result("t", body).unwrap();
        assert_eq!(parsed, Value::String("plain string here".into()));
    }

    #[test]
    fn no_result_frame_is_an_error() {
        assert!(parse_tool_result("t", "data: {\"jsonrpc\":\"2.0\",\"id\":1}\n").is_err());
        assert!(parse_tool_result("t", "").is_err());
    }

    /// `isError:true` used to come back as ordinary data, so `hs status`
    /// rendered a failure message as if it were the status snapshot.
    #[test]
    fn an_is_error_result_is_an_error_carrying_the_tool_message() {
        let body = format!(
            "data: {}\n",
            json!({"jsonrpc": "2.0", "id": 99, "result": {
                "content": [{"type": "text", "text": "storage backend unreachable"}],
                "isError": true}})
        );
        let err = parse_tool_result("system_status", &body).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("storage backend unreachable"), "{msg}");
        assert!(msg.contains("system_status"), "{msg}");
    }

    #[test]
    fn a_json_rpc_error_frame_is_an_error() {
        let body = format!(
            "data: {}\n",
            json!({"jsonrpc": "2.0", "id": 99, "error": {"code": -32602, "message": "unknown tool"}})
        );
        let err = parse_tool_result("nope", &body).unwrap_err();
        assert!(format!("{err:#}").contains("unknown tool"), "{err:#}");
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error_even_when_the_body_looks_like_a_result() {
        // A proxy error page, or an error status with a result-shaped body,
        // must not be returned as the tool's data.
        let server = mcp_server(
            sse(tool_ok("{\"catalog_entries\":1}")).with_status(503),
            200,
        )
        .await;
        let client = client(&server).await.unwrap();

        let err = client
            .call_tool("system_status", json!({}))
            .await
            .expect_err("a 503 is a failure");

        assert!(format!("{err:#}").contains("503"), "{err:#}");
    }

    #[tokio::test]
    async fn call_tool_returns_the_parsed_result() {
        let server = mcp_server(sse(tool_ok("{\"catalog_entries\":2855}")), 200).await;
        let client = client(&server).await.unwrap();

        let value = client.call_tool("system_status", json!({})).await.unwrap();

        assert_eq!(value["catalog_entries"], 2855);
        // Every request after initialize carries the session id.
        let requests = server.requests();
        let call = requests
            .iter()
            .find(|r| r.body.windows(10).any(|w| w == b"tools/call"))
            .unwrap();
        assert_eq!(call.header("mcp-session-id"), Some("sess-1"));
    }

    #[tokio::test]
    async fn a_failed_initialized_notification_fails_the_handshake() {
        let server = FakeServer::start(|req| {
            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            match rpc["method"].as_str() {
                Some("initialize") => sse(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
                    .with_header("mcp-session-id", "sess-1"),
                _ => Response::raw(500, "text/plain", "boom"),
            }
        })
        .await;

        let err = client(&server)
            .await
            .err()
            .expect("the handshake is not complete without the notification");

        assert!(format!("{err:#}").contains("500"), "{err:#}");
    }

    #[tokio::test]
    async fn a_rejected_initialize_fails_with_its_status() {
        let server = FakeServer::start(|_| Response::raw(401, "text/plain", "login required")).await;

        let err = client(&server).await.err().expect("401 on initialize");

        let msg = format!("{err:#}");
        assert!(msg.contains("401") && msg.contains("login required"), "{msg}");
    }

    /// RA-25 (client half): one session per `hs status` run was left behind.
    #[tokio::test]
    async fn close_deletes_the_session_it_opened() {
        let server = mcp_server(sse(tool_ok("{}")), 200).await;
        let client = client(&server).await.unwrap();

        client.close().await.unwrap();

        let deletes: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].path, "/mcp");
        assert_eq!(deletes[0].header("mcp-session-id"), Some("sess-1"));
    }

    #[tokio::test]
    async fn a_failed_session_delete_is_reported_not_swallowed() {
        let server = mcp_server(sse(tool_ok("{}")), 500).await;
        let client = client(&server).await.unwrap();

        let err = client.close().await.expect_err("500 on DELETE");

        assert!(format!("{err:#}").contains("500"), "{err:#}");
    }

    #[tokio::test]
    async fn a_server_without_sessions_has_nothing_to_close() {
        let server = FakeServer::start(|req| {
            let rpc: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            match rpc["method"].as_str() {
                Some("initialize") => sse(json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
                _ => Response::raw(202, "text/plain", ""),
            }
        })
        .await;
        let client = client(&server).await.unwrap();

        client.close().await.unwrap();

        assert!(server.requests().iter().all(|r| r.method != "DELETE"));
    }
}
