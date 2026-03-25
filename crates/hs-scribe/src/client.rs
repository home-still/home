use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

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
        Self {
            http: Client::new(),
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
}
