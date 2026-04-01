use std::time::Duration;

use anyhow::{Context, Result};
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

#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub layout_model: bool,
    pub table_model: bool,
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

    pub async fn health(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.server_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Failed to reach server")?;
        resp.json().await.context("Invalid health response")
    }

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
