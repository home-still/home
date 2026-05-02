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
//!      fire-and-forget a `notifications/initialized`.
//!   3. `call_tool(name, args)` POSTs a `tools/call` request and returns the
//!      tool's text content parsed as JSON (tools always return JSON strings
//!      inside a text content frame, at least in this project's server).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use hs_common::auth::client::AuthenticatedClient;

pub struct McpClient {
    http: reqwest::Client,
    endpoint: String,
    session_id: Option<String>,
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
            let mut client = Self {
                http,
                endpoint: direct,
                session_id: None,
            };
            client.handshake().await?;
            return Ok(client);
        }

        let auth = AuthenticatedClient::from_default_path()
            .context("load cloud credentials (hs cloud enroll --gateway <url>)")?;
        let http = auth
            .build_reqwest_client()
            .await
            .context("build authenticated http client for MCP")?;
        let endpoint = format!("{}/mcp", auth.gateway_url().trim_end_matches('/'));
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
            anyhow::bail!("MCP initialize failed ({status}): {body}");
        }
        self.session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        // Drain body, session is ready once we've read the headers.
        let _ = resp.text().await;

        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self.post(&notif).await;
        Ok(())
    }

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
        Ok(resp.text().await.unwrap_or_default())
    }

    /// Invoke a tool and return its text content parsed as JSON.
    ///
    /// If the tool's text payload isn't valid JSON (some tools return plain
    /// strings), returns it wrapped in `Value::String`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let body = self.post(&req).await?;
        parse_mcp_result_text(&body).ok_or_else(|| {
            anyhow!(
                "no tool result in MCP response for '{name}': body={}",
                body.chars().take(500).collect::<String>()
            )
        })
    }
}

/// Pull the first `data:` SSE frame with a `result.content[0].text` field and
/// parse that text as JSON. Returns `None` if the shape doesn't match.
fn parse_mcp_result_text(body: &str) -> Option<Value> {
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        // Error payload (isError:true) is still surfaced as content — caller can decide.
        let Some(text) = v.pointer("/result/content/0/text").and_then(|t| t.as_str()) else {
            continue;
        };
        return Some(
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string())),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_result_json_text() {
        let body = "data: \n\nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"catalog_entries\\\":2855}\"}],\"isError\":false}}\n";
        let parsed = parse_mcp_result_text(body).unwrap();
        assert_eq!(parsed["catalog_entries"], 2855);
    }

    #[test]
    fn returns_string_for_non_json_text() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"plain string here\"}]}}\n";
        let parsed = parse_mcp_result_text(body).unwrap();
        assert_eq!(parsed, Value::String("plain string here".into()));
    }

    #[test]
    fn returns_none_when_no_result_frame() {
        assert!(parse_mcp_result_text("data: {\"jsonrpc\":\"2.0\",\"id\":1}\n").is_none());
        assert!(parse_mcp_result_text("").is_none());
    }
}
