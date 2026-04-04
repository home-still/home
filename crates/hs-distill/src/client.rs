use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
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
    pub compute_device: String,
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

    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.server_url);
        let resp = self
            .http
            .get(&url)
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

    /// Index a markdown file (non-streaming).
    pub async fn index_file(&self, markdown_path: &str) -> Result<IndexResult> {
        let url = format!("{}/distill", self.server_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "path": markdown_path }))
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

    /// Index a markdown file with streaming progress via NDJSON.
    pub async fn index_file_with_progress(
        &self,
        markdown_path: &str,
        on_progress: impl Fn(DistillProgress),
    ) -> Result<IndexResult> {
        let url = format!("{}/distill/stream", self.server_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "path": markdown_path }))
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
            .send()
            .await
            .context("Failed to reach distill server")?;
        resp.json().await.context("Invalid status response")
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
