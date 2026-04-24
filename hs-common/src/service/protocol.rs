use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Generic NDJSON stream line. Each service provides its own progress and result types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamLine<P, R> {
    Progress(P),
    Result(R),
    Error(String),
}

/// Extract readiness info for pool-based server selection.
pub trait ReadinessInfo {
    fn is_ready(&self) -> bool;
    fn available_slots(&self) -> usize;
}

/// Common service client interface for health/readiness checks.
#[async_trait]
pub trait ServiceClient: Send + Sync {
    type Health: DeserializeOwned;
    type Readiness: DeserializeOwned + ReadinessInfo;

    fn url(&self) -> &str;
    async fn health(&self) -> Result<Self::Health>;
    async fn readiness(&self) -> Result<Self::Readiness>;
}

/// Helper to parse an NDJSON stream from a reqwest response.
/// Calls `on_progress` for each progress event and returns the final result.
pub async fn read_ndjson_stream<P, R>(
    mut resp: reqwest::Response,
    on_progress: impl Fn(P),
) -> Result<R>
where
    P: DeserializeOwned,
    R: DeserializeOwned,
{
    let mut buf = Vec::new();
    while let Some(bytes) = resp.chunk().await.context("Stream read error")? {
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<StreamLine<P, R>>(line) {
                Ok(StreamLine::Progress(event)) => on_progress(event),
                Ok(StreamLine::Result(result)) => return Ok(result),
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
