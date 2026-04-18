use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
use reqwest::Client;
use serde::{Deserialize, Serialize};

// ── NDJSON streaming protocol types ──────────────────────────────

/// A single line in the NDJSON progress stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamLine {
    Progress(ProgressEvent),
    Result { markdown: String },
    Error(String),
}

/// Progress update emitted during PDF processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub stage: String,
    pub page: u64,
    pub total_pages: u64,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub layout_model: bool,
    pub table_model: bool,
    /// Why `layout_model` is false (file path missing, load error, mode
    /// disabled). `None` when the model loaded successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_model_reason: Option<String>,
    /// Why `table_model` is false. Same semantics as `layout_model_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_model_reason: Option<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_utilization_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_used_mb: Option<u64>,
    /// RFC 3339 timestamp of the most recent successful conversion. `None`
    /// when the server has not produced non-empty markdown since startup.
    /// Reflects "processor returned non-empty markdown" — quality validation
    /// (stub-PDF detection, schema checks) lives in callers, not here.
    ///
    /// **In-memory only.** Held by the scribe server as an `AtomicU64`
    /// initialized at process start; a restart resets it to `None` until
    /// the first post-restart conversion. This is diagnostic, not
    /// persistent. For the persistent last-activity timestamp across
    /// restarts, read `system_status.history` — it surfaces the most
    /// recent `Convert`/`Embed` event straight out of the catalog YAMLs.
    ///
    /// A `null` here paired with recent converts in `system_status.history`
    /// is not a contradiction — it just means scribe was restarted between
    /// the last activity and the health probe.
    #[serde(default)]
    pub last_conversion_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResponse {
    pub ready: bool,
    pub vlm_slots_total: usize,
    pub vlm_slots_available: usize,
    pub in_flight_conversions: usize,
}

impl ReadinessInfo for ReadinessResponse {
    fn is_ready(&self) -> bool {
        self.ready
    }
    fn available_slots(&self) -> usize {
        self.vlm_slots_available
    }
}

pub struct ScribeClient {
    http: Client,
    server_url: String,
}

impl ScribeClient {
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

    pub fn url(&self) -> &str {
        &self.server_url
    }

    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.server_url);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed to reach server")?;
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
            .context("Failed to reach server")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Old server without /readiness — treat as always ready
            return Ok(ReadinessResponse {
                ready: true,
                vlm_slots_total: 0,
                vlm_slots_available: 1,
                in_flight_conversions: 0,
            });
        }
        resp.json().await.context("Invalid readiness response")
    }
}

#[async_trait]
impl ServiceClient for ScribeClient {
    type Health = HealthResponse;
    type Readiness = ReadinessResponse;

    fn url(&self) -> &str {
        &self.server_url
    }

    async fn health(&self) -> Result<Self::Health> {
        ScribeClient::health(self).await
    }

    async fn readiness(&self) -> Result<Self::Readiness> {
        ScribeClient::readiness(self).await
    }
}

impl ScribeClient {
    pub async fn convert(&self, pdf_bytes: Vec<u8>) -> Result<String> {
        let url = format!("{}/scribe", self.server_url);
        let part = reqwest::multipart::Part::bytes(pdf_bytes).file_name("input.pdf");
        let form = reqwest::multipart::Form::new().part("pdf", part);

        let resp = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("Failed to send PDF")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        resp.text().await.context("Failed to read response")
    }

    /// Convert a PDF with streaming progress updates via NDJSON.
    /// Falls back to the plain `/scribe` endpoint if the server doesn't
    /// support streaming (404).
    pub async fn convert_with_progress(
        &self,
        pdf_bytes: Vec<u8>,
        on_progress: impl Fn(ProgressEvent),
    ) -> Result<String> {
        let url = format!("{}/scribe/stream", self.server_url);
        let part = reqwest::multipart::Part::bytes(pdf_bytes.clone()).file_name("input.pdf");
        let form = reqwest::multipart::Form::new().part("pdf", part);

        let mut resp = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("Failed to send PDF")?;

        // Server doesn't support streaming — fall back to plain endpoint
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            on_progress(ProgressEvent {
                stage: "info".into(),
                page: 0,
                total_pages: 0,
                message: "server does not support progress (update server image)".into(),
            });
            return self.convert(pdf_bytes).await;
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        // Read chunks and parse NDJSON lines
        let mut buf = Vec::new();
        while let Some(bytes) = resp.chunk().await.context("Stream read error")? {
            buf.extend_from_slice(&bytes);
            // Process complete lines
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<StreamLine>(line) {
                    Ok(StreamLine::Progress(event)) => on_progress(event),
                    Ok(StreamLine::Result { markdown }) => return Ok(markdown),
                    Ok(StreamLine::Error(msg)) => {
                        anyhow::bail!("Server error: {msg}");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse stream line: {e}");
                    }
                }
            }
        }

        anyhow::bail!("Server closed connection without sending result")
    }
}
