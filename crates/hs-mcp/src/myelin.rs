//! The memory tools: hs-mcp serves every tool of myelin's agent surface as
//! its own, so a client connects to one server.
//!
//! At startup [`MyelinTools::connect`] asks the memory server for its tool
//! list and keeps it; nothing here names a myelin tool. A call to one of
//! those tools opens a session to the memory server, forwards the arguments
//! as they came, and returns the server's answer as it came: a result with
//! its content, structured content and `isError` untouched, or the JSON-RPC
//! error it answered. One session per call means a restarted memory server
//! needs no reconnect logic here.
//!
//! The agent surface on loopback takes no token, as `hs-distill`'s store
//! does not send one: the backend token is the home-still shared secret and
//! the memory server is not a home-still service.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use hs_common::auth::client::AuthedHttp;
use hs_common::mcp_client::{McpClient, RawToolOutcome};
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorData, Tool};

/// Longest connecting to the memory server may take.
const MYELIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest one forwarded call may take: `investigate` runs a language model
/// for several steps.
const MYELIN_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// The memory server's tools, and how to call them.
pub struct MyelinTools {
    endpoint: String,
    http: reqwest::Client,
    tools: Vec<Tool>,
}

impl MyelinTools {
    /// Read the tool list of the agent surface at `endpoint`
    /// (`mcp.myelin_url`). A memory server that cannot be reached, that
    /// answers an error, or that offers no tools is an error naming the
    /// URL and the key: there is no server without its memory tools.
    pub async fn connect(endpoint: &str) -> anyhow::Result<Self> {
        let http = http_client()?;
        let session = McpClient::connect(plain(&http), endpoint.to_string())
            .await
            .with_context(|| unreachable_message(endpoint, "open a session"))?;
        let listed = session.list_tools().await;
        session.close_logged().await;
        let listed = listed.with_context(|| unreachable_message(endpoint, "list its tools"))?;
        let mut tools = Vec::with_capacity(listed.len());
        for raw in listed {
            let name = raw
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(no name)")
                .to_string();
            tools.push(serde_json::from_value::<Tool>(raw).map_err(|e| {
                anyhow!(
                    "the memory server at {endpoint} (`mcp.myelin_url`) described its tool \
                     `{name}` in a form hs-mcp cannot read: {e}"
                )
            })?);
        }
        if tools.is_empty() {
            bail!(
                "the memory server at {endpoint} (`mcp.myelin_url`) offers no tools; \
                 is that the agent surface?"
            );
        }
        Self::from_tools(endpoint, tools)
    }

    /// The tools `listed` a memory server at `endpoint` offers.
    pub fn from_tools(endpoint: &str, tools: Vec<Tool>) -> anyhow::Result<Self> {
        let mut seen = BTreeSet::new();
        for tool in &tools {
            if !seen.insert(tool.name.as_ref()) {
                bail!(
                    "the memory server at {endpoint} (`mcp.myelin_url`) lists the tool `{}` twice",
                    tool.name
                );
            }
        }
        Ok(Self {
            endpoint: endpoint.to_string(),
            http: http_client()?,
            tools,
        })
    }

    /// Refuse a tool of the memory server that has the name of one of
    /// `own`: a client could not tell which one it called.
    pub fn refuse_collisions<'a>(
        &self,
        own: impl IntoIterator<Item = &'a str>,
    ) -> anyhow::Result<()> {
        for name in own {
            if self.get(name).is_some() {
                bail!(
                    "the memory server at {} (`mcp.myelin_url`) offers a tool named `{name}`, \
                     which hs-mcp has too",
                    self.endpoint
                );
            }
        }
        Ok(())
    }

    /// The tools, as the memory server described them.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// Forward one call. The answer is the memory server's, unchanged; a
    /// failure to get one is an error naming the server's URL.
    pub async fn call(&self, request: CallToolRequestParams) -> Result<CallToolResult, ErrorData> {
        let name = request.name.to_string();
        let unreachable = |e: anyhow::Error| {
            ErrorData::internal_error(
                format!(
                    "{}: {e:#}",
                    unreachable_message(&self.endpoint, &format!("call `{name}`"))
                ),
                None,
            )
        };
        let session = McpClient::connect(plain(&self.http), self.endpoint.clone())
            .await
            .map_err(unreachable)?;
        let outcome = session
            .call_tool_raw(&name, request.arguments.map(serde_json::Value::Object))
            .await;
        session.close_logged().await;
        match outcome.map_err(unreachable)? {
            RawToolOutcome::Result(result) => serde_json::from_value::<CallToolResult>(result)
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!(
                            "the memory server at {} answered `{name}` with a result hs-mcp \
                             cannot read: {e}",
                            self.endpoint
                        ),
                        None,
                    )
                }),
            RawToolOutcome::Error(error) => Err(serde_json::from_value::<ErrorData>(error)
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!(
                            "the memory server at {} answered `{name}` with an error hs-mcp \
                             cannot read: {e}",
                            self.endpoint
                        ),
                        None,
                    )
                })?),
        }
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    hs_common::http::client_builder()
        .connect_timeout(MYELIN_CONNECT_TIMEOUT)
        .timeout(MYELIN_CALL_TIMEOUT)
        .build()
        .context("building the HTTP client for the memory server")
}

/// No token: see the module comment.
fn plain(http: &reqwest::Client) -> AuthedHttp {
    AuthedHttp::plain_with_backend_token(http.clone(), None)
}

fn unreachable_message(endpoint: &str, doing: &str) -> String {
    format!(
        "hs-mcp could not {doing} on the memory server at {endpoint} (`mcp.myelin_url` in \
         ~/.home-still/config.yaml)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{FakeMyelin, FAKE_MYELIN_ERROR_TOOL, FAKE_MYELIN_REFUSING_TOOL};
    use hs_common::auth::backend::BackendToken;
    use serde_json::{json, Value};
    use std::time::Duration;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// The real `HomeStillMcp` handler, served over HTTP as in production
    /// and read through a client that sees only what a client sees.
    struct Served {
        client: McpClient,
    }

    async fn serve(fake: &FakeMyelin) -> Served {
        let router = crate::http_app::build(
            fake.server().await.unwrap(),
            BackendToken::new(SECRET).unwrap(),
            Duration::from_secs(60),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let http = AuthedHttp::plain_with_backend_token(
            reqwest::Client::new(),
            Some(BackendToken::new(SECRET).unwrap()),
        );
        Served {
            client: McpClient::connect(http, url).await.unwrap(),
        }
    }

    /// Ask the stand-in itself, with no home-still in between.
    async fn direct(fake: &FakeMyelin, tool: &str, arguments: Option<Value>) -> RawToolOutcome {
        let session = McpClient::connect(plain(&http_client().unwrap()), fake.url.clone())
            .await
            .unwrap();
        let outcome = session.call_tool_raw(tool, arguments).await.unwrap();
        session.close_logged().await;
        outcome
    }

    const FAKE_TOOLS: [&str; 3] = ["recall", FAKE_MYELIN_ERROR_TOOL, FAKE_MYELIN_REFUSING_TOOL];

    #[tokio::test]
    async fn the_tool_list_has_the_home_still_tools_and_the_memory_servers() {
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let served = serve(&fake).await;

        let listed = served.client.list_tools().await.unwrap();

        let names: BTreeSet<&str> = listed.iter().filter_map(|t| t["name"].as_str()).collect();
        for own in ["cite", "read", "documents", "status"] {
            assert!(names.contains(own), "{own} is missing from {names:?}");
        }
        for forwarded in FAKE_TOOLS {
            assert!(
                names.contains(forwarded),
                "{forwarded} is missing from {names:?}"
            );
        }
        assert_eq!(names.len(), listed.len(), "no name is listed twice");
        // The definition is the memory server's: description, schema and
        // annotations as it gave them.
        let recall = listed.iter().find(|t| t["name"] == "recall").unwrap();
        assert_eq!(recall["description"], "the stand-in's recall");
        assert_eq!(
            recall["inputSchema"]["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(recall["annotations"]["readOnlyHint"], true);
    }

    #[tokio::test]
    async fn a_forwarded_call_is_answered_with_the_memory_servers_result_unchanged() {
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let served = serve(&fake).await;
        let arguments = json!({"query": "attention", "stores": ["academic", "web"], "k": 5});

        let forwarded = served
            .client
            .call_tool_raw("recall", Some(arguments.clone()))
            .await
            .unwrap();

        assert_eq!(
            forwarded,
            direct(&fake, "recall", Some(arguments.clone())).await
        );
        let RawToolOutcome::Result(result) = forwarded else {
            panic!("a result was expected: {forwarded:?}");
        };
        assert_eq!(result["structuredContent"]["tool"], "recall");
        assert_eq!(result["structuredContent"]["arguments"], arguments);
        assert_eq!(result["content"][0]["text"], "ran recall");
    }

    #[tokio::test]
    async fn a_result_flagged_is_error_stays_one() {
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let served = serve(&fake).await;

        let forwarded = served
            .client
            .call_tool_raw(FAKE_MYELIN_ERROR_TOOL, Some(json!({})))
            .await
            .unwrap();

        assert_eq!(
            forwarded,
            direct(&fake, FAKE_MYELIN_ERROR_TOOL, Some(json!({}))).await
        );
        let RawToolOutcome::Result(result) = forwarded else {
            panic!("a result was expected: {forwarded:?}");
        };
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "refused: only a soft forget is allowed"
        );
    }

    #[tokio::test]
    async fn a_json_rpc_error_from_the_memory_server_is_returned_as_it_came() {
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let served = serve(&fake).await;

        let forwarded = served
            .client
            .call_tool_raw(FAKE_MYELIN_REFUSING_TOOL, Some(json!({})))
            .await
            .unwrap();

        assert_eq!(
            forwarded,
            direct(&fake, FAKE_MYELIN_REFUSING_TOOL, Some(json!({}))).await
        );
        let RawToolOutcome::Error(error) = forwarded else {
            panic!("an error was expected: {forwarded:?}");
        };
        assert_eq!(error["code"], -32602);
        assert_eq!(
            error["message"],
            "name a store; no default store is defined"
        );
    }

    #[tokio::test]
    async fn the_handler_finds_a_forwarded_tool_by_name() {
        use rmcp::ServerHandler;
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let server = fake.server().await.unwrap();

        assert!(server.get_tool("recall").is_some());
        assert!(server.get_tool("cite").is_some());
        assert!(server.get_tool("no_such_tool").is_none());
    }

    #[tokio::test]
    async fn startup_fails_naming_the_url_and_the_key_when_nothing_listens() {
        // A port that was just free: nothing listens on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/mcp");

        let err = MyelinTools::connect(&url)
            .await
            .err()
            .expect("nothing listens");

        let msg = format!("{err:#}");
        assert!(msg.contains(&url), "{msg}");
        assert!(msg.contains("mcp.myelin_url"), "{msg}");
    }

    #[tokio::test]
    async fn startup_fails_when_the_memory_server_offers_no_tools() {
        let fake = FakeMyelin::start(&[]).await;

        let msg = format!("{:#}", MyelinTools::connect(&fake.url).await.err().unwrap());

        assert!(
            msg.contains("offers no tools") && msg.contains(&fake.url),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn a_tool_name_that_home_still_has_too_stops_startup() {
        let fake = FakeMyelin::start(&["recall", "cite"]).await;

        let err = fake
            .server()
            .await
            .err()
            .expect("a collision must stop startup");

        let msg = format!("{err:#}");
        assert!(msg.contains("`cite`") && msg.contains(&fake.url), "{msg}");
    }

    #[tokio::test]
    async fn a_call_when_the_memory_server_is_gone_is_an_error_naming_its_url() {
        let fake = FakeMyelin::start(&FAKE_TOOLS).await;
        let tools = MyelinTools::connect(&fake.url).await.unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let gone = format!("http://127.0.0.1:{port}/mcp");
        let tools = MyelinTools::from_tools(&gone, tools.tools().to_vec()).unwrap();

        let err = tools
            .call(CallToolRequestParams::new("recall"))
            .await
            .expect_err("nobody answers");

        assert!(err.message.contains(&gone), "{}", err.message);
        assert!(err.message.contains("recall"), "{}", err.message);
    }
}
