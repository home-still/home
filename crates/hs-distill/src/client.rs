use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::auth::client::AuthedHttp;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
use hs_common::storage::Storage;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

// ── Protocol types ─────────────────────────────────────────────

/// Progress update emitted during indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistillProgress {
    pub stage: String,
    pub doc: String,
    pub chunks_done: u64,
    pub chunks_total: u64,
    pub message: String,
}

/// Most hits one search request may ask for; larger `limit`s are clamped.
pub const MAX_SEARCH_LIMIT: u64 = 200;

/// Most document ids `/docs` returns; a larger `limit` is rejected (HTTP
/// 400). Also the cap on the distinct-document count in `/status`.
pub const MAX_DOC_LIST_LIMIT: u64 = 1_000_000;

/// Result of indexing a single document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub doc_id: String,
    pub chunks_indexed: u32,
    pub embedding_device: String,
}

/// A single search hit returned to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub doc_id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub year: Option<u64>,
    #[serde(default)]
    pub doi: Option<String>,
    pub chunk_text: String,
    pub score: f32,
    pub pdf_path: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    pub page: Option<usize>,
    /// Personal-store category. Always `None` for academic-papers hits;
    /// populated only on hits originating from the personal pipeline.
    #[serde(default)]
    pub category: Option<String>,
}

/// Search filters for the /search endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchFilters {
    pub year: Option<String>,
    pub topic: Option<String>,
    /// Restrict hits to a single personal-store category.
    #[serde(default)]
    pub category: Option<String>,
}

/// Health response from the distill server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub compute_device: String,
    pub collection: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub qdrant_version: String,
    #[serde(default)]
    pub embed_model: String,
}

/// Readiness response from the distill server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResponse {
    /// The embedder is healthy and Qdrant answers.
    pub ready: bool,
    pub in_flight: usize,
    /// Concurrent `embed` calls the server can run (its model pool size).
    #[serde(default = "default_capacity")]
    pub capacity: usize,
    /// Why `ready` is false.
    #[serde(default)]
    pub reason: Option<String>,
}

fn default_capacity() -> usize {
    1
}

impl ReadinessInfo for ReadinessResponse {
    fn is_ready(&self) -> bool {
        self.ready
    }
    fn available_slots(&self) -> usize {
        if self.ready {
            self.capacity.saturating_sub(self.in_flight)
        } else {
            0
        }
    }
}

/// Status response from the distill server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub collection: String,
    pub points_count: u64,
    #[serde(default)]
    pub documents_count: u64,
    /// `documents_count` stopped at [`MAX_DOC_LIST_LIMIT`]; more exist.
    #[serde(default)]
    pub documents_count_truncated: bool,
    pub compute_device: String,
    #[serde(default)]
    pub embed_model: String,
    /// Qdrant endpoint this distill server is talking to. An internal
    /// address, so it is served only on the token-protected `/status`, never
    /// on the open `/health`.
    #[serde(default)]
    pub qdrant_url: String,
}

pub type DistillStreamLine = hs_common::service::protocol::StreamLine<DistillProgress, IndexResult>;

// ── Client ─────────────────────────────────────────────────────

/// A non-2xx answer from the distill server. Typed so callers can tell a
/// request the server will never accept (bad input) from an outage without
/// matching on message text.
#[derive(Debug, Clone)]
pub struct ServerError {
    pub status: reqwest::StatusCode,
    pub body: String,
    /// `x-hs-error-code` header, when the server sent one.
    pub code: Option<String>,
}

/// Realm the distill server's token middleware names in `WWW-Authenticate`.
/// Only a 401 carrying it proves the rejection came from the distill server
/// itself; a bare 401 or a 403 can come from a proxy or gateway.
pub const AUTH_REALM: &str = "hs-distill";

/// The distill server rejected our credentials (a 401 naming
/// [`AUTH_REALM`]): `HS_BACKEND_TOKEN` is missing, wrong, or not the value
/// the server was started with. A configuration error, not an outage —
/// retrying cannot succeed. Any other 401, and 403, are ordinary
/// [`ServerError`]s.
#[derive(Debug, Clone)]
pub struct Unauthorized {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "distill server rejected our credentials ({}): check HS_BACKEND_TOKEN matches the server's: {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for Unauthorized {}

impl ServerError {
    /// The server understood the request and refuses it for what it
    /// contains (400 bad input, 413 too large, 422 unprocessable) or crashed
    /// on it (`index_panicked`): sending it again cannot succeed. Everything
    /// else — 5xx, 404/405 during a deploy, 408/429 — may clear up. 401/403
    /// are [`Unauthorized`], a separate type.
    pub fn is_permanent_rejection(&self) -> bool {
        matches!(self.status.as_u16(), 400 | 413 | 422)
            || self.code.as_deref() == Some(crate::api::PANIC_CODE)
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Server error {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ServerError {}

/// Upper bound on one indexing request (`/distill`, `/distill/stream`) unless
/// the caller sets another with [`DistillClient::with_index_timeout`].
///
/// Indexing a book-length markdown is chunk -> GPU embed -> upsert on a card
/// shared with the scribe VLM: minutes under contention, never close to
/// half an hour. The bound exists for the stalled-server case, where a
/// request with no deadline holds its handler slot for as long as the
/// event's `ack_wait` (default 7200 s), after which the broker redelivers
/// the event and a second worker indexes the same document concurrently.
/// 1800 s leaves >5000 s of that window for the stamp and publish that
/// follow.
pub const DEFAULT_INDEX_TIMEOUT: Duration = Duration::from_secs(1800);

/// Bound for every request that is not an index, a scrub or a reset and does
/// not set its own: the client-wide default, so no request site can be left
/// without one.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct DistillClient {
    http: AuthedHttp,
    server_url: String,
    index_timeout: Duration,
}

impl DistillClient {
    pub fn new(server_url: &str) -> Result<Self> {
        let http = hs_common::http::client_builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            // Match ScribeClient's jitter tolerance: catch half-open TCP
            // in ~30 s, not the kernel's default ~2 h.
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .context("failed to build DistillClient reqwest Client")?;
        Ok(Self::new_with_client(server_url, AuthedHttp::plain(http)))
    }

    /// Create a client over a pre-built [`AuthedHttp`] (cloud gateway: the
    /// token is attached per request).
    pub fn new_with_client(server_url: &str, http: AuthedHttp) -> Self {
        Self {
            http,
            server_url: server_url.trim_end_matches('/').to_string(),
            index_timeout: DEFAULT_INDEX_TIMEOUT,
        }
    }

    /// Bound each indexing request at `timeout` instead of
    /// [`DEFAULT_INDEX_TIMEOUT`].
    pub fn with_index_timeout(mut self, timeout: Duration) -> Self {
        self.index_timeout = timeout;
        self
    }

    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.server_url);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed to reach distill server")?;
        json_or_server_error(resp, "health").await
    }

    /// `/readiness` answers 503 with a JSON body (`ready: false`, `reason`)
    /// when the server is up but not ready; that body is the answer, so it
    /// is parsed whatever the status.
    pub async fn readiness(&self) -> Result<ReadinessResponse> {
        let url = format!("{}/readiness", self.server_url);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .context("Failed to reach distill server")?;
        resp.json().await.context("Invalid readiness response")
    }

    /// Index a markdown file (non-streaming). Reads the file locally and sends content to server.
    pub async fn index_file(&self, markdown_path: &str) -> Result<IndexResult> {
        let content = std::fs::read_to_string(markdown_path)
            .context(format!("Failed to read {markdown_path}"))?;
        self.index_content(markdown_path, &content, None).await
    }

    /// Index markdown already held in memory. `path_hint` names the document
    /// (its file stem becomes the doc id) but the server never reads it from
    /// disk. Pass `catalog` when the caller already has the catalog entry:
    /// the server has no catalog of its own, so without one the Qdrant
    /// payload carries no title, authors, DOI or year.
    pub async fn index_content(
        &self,
        path_hint: &str,
        content: &str,
        catalog: Option<&hs_common::catalog::CatalogEntry>,
    ) -> Result<IndexResult> {
        self.index_content_in(path_hint, content, catalog, None)
            .await
    }

    /// Same as `index_content` but routes the upsert to a non-default Qdrant
    /// collection. The collection must be one the server is configured to
    /// serve (`distill_server.collections`); an unknown name is a 400. The
    /// request is bounded by the client's index timeout.
    pub async fn index_content_in(
        &self,
        path_hint: &str,
        content: &str,
        catalog: Option<&hs_common::catalog::CatalogEntry>,
        collection: Option<&str>,
    ) -> Result<IndexResult> {
        let url = format!("{}/distill", self.server_url);
        let body = index_body(path_hint, content, catalog, collection)?;
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .timeout(self.index_timeout)
            .send()
            .await
            .context("Failed to send index request")?;

        json_or_server_error(resp, "index").await
    }

    /// Index a markdown object pulled from a `Storage` backend (local or S3).
    pub async fn index_from_storage(
        &self,
        storage: &dyn Storage,
        key: &str,
    ) -> Result<IndexResult> {
        self.index_from_storage_with_catalog(storage, key, None)
            .await
    }

    /// Same as `index_from_storage` but forwards a catalog entry loaded by
    /// the caller (the server cannot look one up itself).
    pub async fn index_from_storage_with_catalog(
        &self,
        storage: &dyn Storage,
        key: &str,
        catalog: Option<&hs_common::catalog::CatalogEntry>,
    ) -> Result<IndexResult> {
        let bytes = storage
            .get(key)
            .await
            .with_context(|| format!("Failed to read {key} from storage"))?;
        let content = String::from_utf8(bytes)
            .with_context(|| format!("Markdown at {key} is not valid UTF-8"))?;
        self.index_content(key, &content, catalog).await
    }

    /// Index a markdown file with streaming progress via NDJSON.
    /// Reads the file locally and sends content to the server. Bounded by
    /// the client's index timeout (the whole stream, not each line).
    pub async fn index_file_with_progress(
        &self,
        markdown_path: &str,
        on_progress: impl Fn(DistillProgress),
    ) -> Result<IndexResult> {
        let content = std::fs::read_to_string(markdown_path)
            .context(format!("Failed to read {markdown_path}"))?;
        let url = format!("{}/distill/stream", self.server_url);
        let resp = self
            .http
            .post(&url)
            .json(&index_body(markdown_path, &content, None, None)?)
            .timeout(self.index_timeout)
            .send()
            .await
            .context("Failed to send index request")?;

        let resp = ensure_success(resp).await?;

        hs_common::service::protocol::read_ndjson_stream(resp, on_progress)
            .await
            .map_err(|e| type_stream_error(e))
    }

    /// Search indexed documents.
    pub async fn search(
        &self,
        query: &str,
        limit: u64,
        filters: SearchFilters,
    ) -> Result<Vec<SearchHit>> {
        self.search_in(query, limit, filters, None).await
    }

    /// Same as `search` but targets a non-default collection (e.g.
    /// `personal_docs`), which must be one the server is configured to serve.
    /// The server clamps `limit` to [`MAX_SEARCH_LIMIT`].
    pub async fn search_in(
        &self,
        query: &str,
        limit: u64,
        filters: SearchFilters,
        collection: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let url = format!("{}/search", self.server_url);
        let mut body = serde_json::json!({
            "query": query,
            "limit": limit,
            "filters": filters,
        });
        if let Some(c) = collection {
            body["collection"] = serde_json::Value::String(c.to_string());
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to send search request")?;

        json_or_server_error(resp, "search").await
    }

    /// Get collection status.
    pub async fn status(&self) -> Result<StatusResponse> {
        let url = format!("{}/status", self.server_url);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed to reach distill server")?;
        json_or_server_error(resp, "status").await
    }

    /// Check if a document is already indexed.
    pub async fn doc_exists(&self, doc_id: &str) -> Result<bool> {
        Ok(self.doc_chunks(doc_id).await?.0)
    }

    /// Return `(exists, chunk_count)` for a doc. Chunk count is needed by the
    /// reconciler to backfill catalog stamps that were lost on earlier writes.
    pub async fn doc_chunks(&self, doc_id: &str) -> Result<(bool, u64)> {
        #[derive(Deserialize)]
        struct Exists {
            exists: bool,
            chunks: u64,
        }
        let url = format!("{}/exists/{}", self.server_url, doc_id);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: Exists = json_or_server_error(resp, "exists").await?;
        Ok((data.exists, data.chunks))
    }

    /// Delete every point whose `doc_id` matches. Returns the number of
    /// points that were deleted.
    pub async fn delete_doc(&self, doc_id: &str) -> Result<u64> {
        self.delete_doc_in(doc_id, None).await
    }

    /// Same as `delete_doc` but targets a non-default collection.
    pub async fn delete_doc_in(&self, doc_id: &str, collection: Option<&str>) -> Result<u64> {
        #[derive(Deserialize)]
        struct Deleted {
            deleted: u64,
        }
        let mut url = format!("{}/doc/{}", self.server_url, doc_id);
        if let Some(c) = collection {
            url.push_str(&format!("?collection={c}"));
        }
        let resp = self
            .http
            .delete(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: Deleted = json_or_server_error(resp, "delete").await?;
        Ok(data.deleted)
    }

    /// Drop + recreate the Qdrant collection, wiping every vector.
    /// Returns the pre-drop point count for reporting.
    pub async fn reset_collection(&self) -> Result<u64> {
        #[derive(Deserialize)]
        struct Reset {
            deleted_points: u64,
        }
        let url = format!("{}/collection/reset", self.server_url);
        let resp = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: Reset = json_or_server_error(resp, "reset_collection").await?;
        Ok(data.deleted_points)
    }

    /// Ask the server to enable HNSW on a collection (default collection when
    /// `None`) with its configured parameters. Returns at once; Qdrant builds
    /// the graph in the background. Idempotent.
    pub async fn enable_hnsw(&self, collection: Option<&str>) -> Result<crate::store::HnswEnable> {
        let mut url = format!("{}/collection/hnsw", self.server_url);
        if let Some(c) = collection {
            url.push_str(&format!("?collection={c}"));
        }
        let resp = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("Failed to reach distill server")?;
        json_or_server_error(resp, "enable_hnsw").await
    }

    /// Scan every point in the collection for chunks whose `chunk_text`
    /// matches a known anti-bot / cookie-banner interstitial signature.
    /// When `dry_run` is true, returns counts and samples without
    /// deleting. Used by the `hs pipeline purge-poisoned-chunks` CLI to
    /// scrub contamination from real papers (cookie banner appended to
    /// the article body) without dropping the whole document.
    pub async fn scrub_interstitials(&self, dry_run: bool) -> Result<crate::types::ScrubReport> {
        let url = format!(
            "{}/scrub-interstitials?dry_run={}",
            self.server_url, dry_run
        );
        let resp = self
            .http
            .post(&url)
            // Scroll over an entire collection can take minutes on a large
            // corpus; let the server set the pace rather than timing it out.
            .timeout(Duration::from_secs(900))
            .send()
            .await
            .context("Failed to reach distill server")?;
        json_or_server_error(resp, "scrub_interstitials").await
    }

    /// List every distinct `doc_id` present in the collection. `limit` is a
    /// safety cap, not a page size: a collection with more documents than
    /// `limit` (or than the server maximum, [`MAX_DOC_LIST_LIMIT`]) is an
    /// error, never a silently partial list — callers diff this list against
    /// storage, and a missing id looks like a document that was never
    /// indexed.
    pub async fn list_docs(&self, limit: u64) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Docs {
            doc_ids: Vec<String>,
            #[serde(default)]
            truncated: bool,
        }
        let limit = limit.min(MAX_DOC_LIST_LIMIT);
        let url = format!("{}/docs?limit={}", self.server_url, limit);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: Docs = json_or_server_error(resp, "list_docs").await?;
        if data.truncated {
            anyhow::bail!(
                "the collection holds more than {limit} documents (server maximum {MAX_DOC_LIST_LIMIT}); \
                 the list would be incomplete"
            );
        }
        Ok(data.doc_ids)
    }
}

/// Body of an index request.
fn index_body(
    path_hint: &str,
    content: &str,
    catalog: Option<&hs_common::catalog::CatalogEntry>,
    collection: Option<&str>,
) -> Result<serde_json::Value> {
    let mut body = serde_json::json!({ "path": path_hint, "content": content });
    if let Some(cat) = catalog {
        body["catalog"] =
            serde_json::to_value(cat).context("Failed to serialize the catalog entry")?;
    }
    if let Some(c) = collection {
        body["collection"] = serde_json::Value::String(c.to_string());
    }
    Ok(body)
}

/// The stream route reports a server-side panic as an error *line* (the
/// status is already 200). Give it the same typed, permanent error the
/// non-stream route's 500 + `x-hs-error-code` produces.
fn type_stream_error(e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    match msg.strip_prefix("Server error: ") {
        Some(rest) if rest.starts_with(crate::api::PANIC_CODE) => ServerError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: rest.to_string(),
            code: Some(crate::api::PANIC_CODE.to_string()),
        }
        .into(),
        _ => e,
    }
}

fn names_distill_realm(www_authenticate: Option<&str>) -> bool {
    www_authenticate.is_some_and(|v| v.contains(&format!("realm=\"{AUTH_REALM}\"")))
}

/// Pass a 2xx response through; turn any other status into a [`ServerError`]
/// carrying the server's message (it explains 400/503/500 precisely).
async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let www_auth = resp
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let code = resp
        .headers()
        .get("x-hs-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp_www_auth = www_auth;
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 && names_distill_realm(resp_www_auth.as_deref()) {
        return Err(Unauthorized { status, body }.into());
    }
    Err(ServerError { status, body, code }.into())
}

/// Decode a 2xx JSON reply into `T`.
async fn json_or_server_error<T: DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
) -> Result<T> {
    ensure_success(resp)
        .await?
        .json()
        .await
        .with_context(|| format!("Invalid {what} response"))
}

#[async_trait]
impl ServiceClient for DistillClient {
    type Health = HealthResponse;
    type Readiness = ReadinessResponse;

    fn url(&self) -> &str {
        &self.server_url
    }

    async fn health(&self) -> Result<Self::Health> {
        DistillClient::health(self).await
    }

    async fn readiness(&self) -> Result<Self::Readiness> {
        DistillClient::readiness(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{serve, Reply};
    use std::time::Instant;

    fn client(fake: &crate::testutil::FakeHttp) -> DistillClient {
        DistillClient::new(&fake.url()).unwrap()
    }

    fn is_timeout(err: &anyhow::Error) -> bool {
        err.chain()
            .filter_map(|c| c.downcast_ref::<reqwest::Error>())
            .any(|e| e.is_timeout())
    }

    #[tokio::test]
    async fn a_stalled_server_cannot_hold_an_index_request_past_its_timeout() {
        // RA-46: no deadline meant the handler slot was held until the
        // event's ack_wait elapsed.
        let fake = serve(|_| Reply::Hang).await;
        let c = client(&fake).with_index_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = c.index_content("doc.md", "text", None).await.unwrap_err();
        assert!(is_timeout(&err), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn the_streaming_index_request_is_bounded_too() {
        let fake = serve(|_| Reply::Hang).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.md");
        std::fs::write(&file, "some markdown").unwrap();
        let c = client(&fake).with_index_timeout(Duration::from_millis(300));
        let err = c
            .index_file_with_progress(file.to_str().unwrap(), |_| {})
            .await
            .unwrap_err();
        assert!(is_timeout(&err), "{err:#}");
    }

    #[tokio::test]
    async fn index_timeout_is_set_per_client() {
        let fake = serve(|_| Reply::Hang).await;
        let default = client(&fake);
        assert_eq!(default.index_timeout, DEFAULT_INDEX_TIMEOUT);
        let custom = client(&fake).with_index_timeout(Duration::from_secs(42));
        assert_eq!(custom.index_timeout, Duration::from_secs(42));
    }

    #[tokio::test]
    async fn index_request_always_carries_content_and_the_catalog() {
        let fake = serve(|_| {
            Reply::Json(
                200,
                r#"{"doc_id":"doc","chunks_indexed":3,"embedding_device":"Cuda"}"#.into(),
            )
        })
        .await;
        let catalog = hs_common::catalog::CatalogEntry {
            title: Some("T".into()),
            ..Default::default()
        };
        let got = client(&fake)
            .index_content_in(
                "markdown/ab/doc.md",
                "# body",
                Some(&catalog),
                Some("personal_docs"),
            )
            .await
            .unwrap();
        assert_eq!(got.chunks_indexed, 3);

        let seen = fake.recorded();
        assert!(seen[0].request_line.starts_with("POST /distill "));
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["content"], "# body");
        assert_eq!(body["path"], "markdown/ab/doc.md");
        assert_eq!(body["collection"], "personal_docs");
        assert_eq!(body["catalog"]["title"], "T");
    }

    #[tokio::test]
    async fn server_errors_surface_the_status_and_message() {
        let fake = serve(|_| Reply::Json(400, "`content` is required".into())).await;
        let err = client(&fake)
            .index_content("doc.md", "x", None)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("400") && msg.contains("`content` is required"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn rejections_are_typed_so_callers_can_tell_bad_input_from_an_outage() {
        for (status, permanent) in [
            (400, true),
            (413, true),
            (422, true),
            (404, false),
            (408, false),
            (429, false),
            (500, false),
            (503, false),
        ] {
            let fake = serve(move |_| Reply::Json(status, "no".into())).await;
            let err = client(&fake)
                .index_content("d.md", "x", None)
                .await
                .unwrap_err();
            let typed = err
                .downcast_ref::<ServerError>()
                .unwrap_or_else(|| panic!("{status}: untyped error {err:#}"));
            assert_eq!(typed.status.as_u16(), status);
            assert_eq!(typed.is_permanent_rejection(), permanent, "{status}");
        }
    }

    #[tokio::test]
    async fn a_404_from_the_stream_endpoint_is_an_error_not_a_second_request() {
        // The old client answered a 404 by silently re-posting to /distill.
        let fake = serve(|_| Reply::Json(404, "not found".into())).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.md");
        std::fs::write(&file, "some markdown").unwrap();
        let err = client(&fake)
            .index_file_with_progress(file.to_str().unwrap(), |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"));
        let seen = fake.recorded();
        assert_eq!(seen.len(), 1, "no fallback request: {seen:?}");
        assert!(seen[0].request_line.contains("/distill/stream"));
    }

    #[tokio::test]
    async fn list_docs_refuses_a_partial_list() {
        let fake =
            serve(|_| Reply::Json(200, r#"{"doc_ids":["a","b"],"truncated":true}"#.into())).await;
        let err = client(&fake).list_docs(2).await.unwrap_err();
        assert!(format!("{err:#}").contains("more than 2"), "{err:#}");

        let fake = serve(|_| Reply::Json(200, r#"{"doc_ids":["a","b"]}"#.into())).await;
        assert_eq!(client(&fake).list_docs(10).await.unwrap(), ["a", "b"]);
    }

    #[tokio::test]
    async fn list_docs_never_asks_for_more_than_the_server_maximum() {
        let fake = serve(|_| Reply::Json(200, r#"{"doc_ids":[]}"#.into())).await;
        client(&fake).list_docs(u64::MAX).await.unwrap();
        let line = &fake.recorded()[0].request_line;
        assert!(
            line.contains(&format!("limit={MAX_DOC_LIST_LIMIT}")),
            "{line}"
        );
    }

    #[tokio::test]
    async fn malformed_or_error_replies_are_errors_not_defaults() {
        // `doc_chunks` used to read a missing `exists` as false.
        let fake = serve(|_| Reply::Json(200, "{}".into())).await;
        assert!(client(&fake).doc_chunks("d").await.is_err());
        let fake = serve(|_| Reply::Json(503, "embedder unusable".into())).await;
        let err = client(&fake).doc_chunks("d").await.unwrap_err();
        assert!(format!("{err:#}").contains("503"));
        let err = client(&fake).health().await.unwrap_err();
        assert!(format!("{err:#}").contains("embedder unusable"), "{err:#}");
        let fake = serve(|_| Reply::Json(200, "{}".into())).await;
        assert!(client(&fake).delete_doc("d").await.is_err());
        assert!(client(&fake).reset_collection().await.is_err());
    }

    #[tokio::test]
    async fn a_503_readiness_body_is_the_answer() {
        let fake = serve(|_| {
            Reply::Json(
                503,
                r#"{"ready":false,"in_flight":0,"capacity":1,"reason":"embedder unusable"}"#.into(),
            )
        })
        .await;
        let r = client(&fake).readiness().await.unwrap();
        assert!(!r.ready);
        assert_eq!(r.reason.as_deref(), Some("embedder unusable"));
    }

    const REALM: &str = "Bearer realm=\"hs-distill\"";

    #[tokio::test]
    async fn only_a_401_naming_the_distill_realm_is_unauthorized() {
        let fake = serve(|_| {
            Reply::JsonWithHeader(
                401,
                r#"{"error":"unauthorized"}"#.into(),
                "www-authenticate",
                REALM,
            )
        })
        .await;
        let c = client(&fake);
        for err in [
            c.index_content("d.md", "x", None).await.unwrap_err(),
            c.status().await.unwrap_err(),
            c.delete_doc("d").await.unwrap_err(),
        ] {
            let u = err
                .downcast_ref::<Unauthorized>()
                .unwrap_or_else(|| panic!("untyped {err:#}"));
            assert_eq!(u.status.as_u16(), 401);
            assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"));
        }
        // A bare 401 (proxy), another realm, and every 403 are ordinary errors.
        for reply in [
            Reply::Json(401, "no".into()),
            Reply::JsonWithHeader(
                401,
                "no".into(),
                "www-authenticate",
                "Bearer realm=\"gateway\"",
            ),
            Reply::Json(403, "no".into()),
            Reply::JsonWithHeader(403, "no".into(), "www-authenticate", REALM),
        ] {
            let slot = std::sync::Mutex::new(Some(reply));
            let fake = serve(move |_| match slot.lock().unwrap().as_ref().unwrap() {
                Reply::Json(s, b) => Reply::Json(*s, b.clone()),
                Reply::JsonWithHeader(s, b, k, v) => Reply::JsonWithHeader(*s, b.clone(), k, v),
                Reply::Hang => Reply::Hang,
            })
            .await;
            let err = client(&fake).status().await.unwrap_err();
            assert!(err.downcast_ref::<Unauthorized>().is_none(), "{err:#}");
            let typed = err.downcast_ref::<ServerError>().expect("ordinary error");
            assert!(!typed.is_permanent_rejection());
        }
    }

    #[test]
    fn a_panic_error_line_on_the_stream_is_the_typed_permanent_error() {
        let typed = type_stream_error(anyhow::anyhow!(
            "Server error: index_panicked: indexing d panicked"
        ));
        let s = typed.downcast_ref::<ServerError>().expect("typed");
        assert!(s.is_permanent_rejection());
        assert_eq!(s.code.as_deref(), Some(crate::api::PANIC_CODE));
        let other = type_stream_error(anyhow::anyhow!("Server error: qdrant down"));
        assert!(other.downcast_ref::<ServerError>().is_none());
    }
}
