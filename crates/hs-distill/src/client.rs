use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
use hs_common::storage::Storage;
use reqwest::Client;
use serde::{Deserialize, Serialize};

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
}

/// Search filters for the /search endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchFilters {
    pub year: Option<String>,
    pub topic: Option<String>,
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
    /// Qdrant endpoint this distill server is talking to, straight from the
    /// server's config. Surfaced so the CLI / MCP snapshot can render the
    /// real Qdrant URL in the dashboard instead of mislabeling distill's own
    /// URL as Qdrant's.
    #[serde(default)]
    pub qdrant_url: String,
}

/// Readiness response from the distill server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResponse {
    pub ready: bool,
    pub in_flight: usize,
}

impl ReadinessInfo for ReadinessResponse {
    fn is_ready(&self) -> bool {
        self.ready
    }
    fn available_slots(&self) -> usize {
        if self.ready {
            1
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
    pub compute_device: String,
    #[serde(default)]
    pub embed_model: String,
}

pub type DistillStreamLine = hs_common::service::protocol::StreamLine<DistillProgress, IndexResult>;

// ── Client ─────────────────────────────────────────────────────

pub struct DistillClient {
    http: Client,
    server_url: String,
}

impl DistillClient {
    pub fn new(server_url: &str) -> Self {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            http,
            server_url: server_url.trim_end_matches('/').to_string(),
        }
    }

    /// Create a client with a pre-configured reqwest Client (e.g., with auth headers).
    pub fn new_with_client(server_url: &str, http: Client) -> Self {
        Self {
            http,
            server_url: server_url.trim_end_matches('/').to_string(),
        }
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
        resp.json().await.context("Invalid health response")
    }

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

    /// Index markdown already held in memory. `path_hint` is used server-side for
    /// logging and catalog lookup (derive the doc stem) but the server never
    /// reads it from disk. Pass `catalog` when the caller already has the
    /// catalog entry so metadata ends up on the Qdrant payload even when the
    /// server has no local catalog directory.
    pub async fn index_content(
        &self,
        path_hint: &str,
        content: &str,
        catalog: Option<&hs_common::catalog::CatalogEntry>,
    ) -> Result<IndexResult> {
        let url = format!("{}/distill", self.server_url);
        let mut body = serde_json::json!({ "path": path_hint, "content": content });
        if let Some(cat) = catalog {
            body["catalog"] = serde_json::to_value(cat).unwrap_or(serde_json::Value::Null);
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send index request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        resp.json().await.context("Invalid index response")
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
    /// the caller (so the server's metadata extraction doesn't have to walk
    /// the filesystem).
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
    /// Reads the file locally and sends content to the server.
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
            .json(&serde_json::json!({ "path": markdown_path, "content": content }))
            .send()
            .await
            .context("Failed to send index request")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return self.index_file(markdown_path).await;
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        hs_common::service::protocol::read_ndjson_stream(resp, on_progress).await
    }

    /// Search indexed documents.
    pub async fn search(
        &self,
        query: &str,
        limit: u64,
        filters: SearchFilters,
    ) -> Result<Vec<SearchHit>> {
        let url = format!("{}/search", self.server_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "query": query,
                "limit": limit,
                "filters": filters,
            }))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to send search request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        resp.json().await.context("Invalid search response")
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
        resp.json().await.context("Invalid status response")
    }

    /// Check if a document is already indexed.
    pub async fn doc_exists(&self, doc_id: &str) -> Result<bool> {
        let url = format!("{}/exists/{}", self.server_url, doc_id);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: serde_json::Value = resp.json().await.context("Invalid exists response")?;
        Ok(data["exists"].as_bool().unwrap_or(false))
    }

    /// Delete every point whose `doc_id` matches. Returns the number of
    /// points that were deleted.
    pub async fn delete_doc(&self, doc_id: &str) -> Result<u64> {
        let url = format!("{}/doc/{}", self.server_url, doc_id);
        let resp = self
            .http
            .delete(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("Failed to reach distill server")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }
        let data: serde_json::Value = resp.json().await.context("Invalid delete response")?;
        Ok(data["deleted"].as_u64().unwrap_or(0))
    }

    /// List every distinct `doc_id` present in the collection.
    pub async fn list_docs(&self, limit: u64) -> Result<Vec<String>> {
        let url = format!("{}/docs?limit={}", self.server_url, limit);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("Failed to reach distill server")?;
        let data: serde_json::Value = resp.json().await.context("Invalid list_docs response")?;
        let ids = data["doc_ids"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }
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
