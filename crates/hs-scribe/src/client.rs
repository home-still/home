use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::TimeoutPolicy;

/// HTTP header name carrying the per-request convert deadline. The
/// server reads this and wraps `process_pdf` in
/// `tokio::time::timeout(header_value)`, so client and server agree on
/// how long to wait for a given PDF instead of drifting between
/// independent config defaults.
pub const CONVERT_DEADLINE_HEADER: &str = "X-Convert-Deadline-Secs";

/// Compute the per-request convert timeout from a PDF's page count.
///
/// `pages = None` (parse failed, non-PDF payload) → `policy.fallback_secs`.
/// Otherwise: `clamp(base + pages * per_page, floor, ceiling)`.
pub fn compute_convert_timeout(pages: Option<u32>, policy: &TimeoutPolicy) -> Duration {
    let secs = match pages {
        None => policy.fallback_secs,
        Some(n) => {
            let raw = policy
                .base_secs
                .saturating_add(policy.per_page_secs.saturating_mul(n as u64));
            raw.clamp(policy.floor_secs, policy.ceiling_secs)
        }
    };
    Duration::from_secs(secs)
}

// ── NDJSON streaming protocol types ──────────────────────────────

/// Result of a successful PDF→markdown conversion. Carries the assembled
/// markdown plus the per-page list of PP-DocLayout-V3 region class names
/// (index-aligned with `compute_page_offsets` on `markdown`). The class
/// list is empty for pages produced by FullPage mode (no layout
/// detection happens) or by the streaming-fallback path that goes through
/// the plain `/scribe` endpoint — both yield an empty `Vec<Vec<String>>`,
/// which downstream QC treats as "not bibliography" (strict default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversionResult {
    pub markdown: String,
    #[serde(default)]
    pub per_page_region_classes: Vec<Vec<String>>,
}

/// A single line in the NDJSON progress stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamLine {
    Progress(ProgressEvent),
    Result {
        markdown: String,
        #[serde(default)]
        per_page_region_classes: Vec<Vec<String>>,
    },
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
    /// Total successful conversions since server startup. Monotonic
    /// counter, cheap atomic increment. Consumers (e.g. `hs scribe
    /// autotune`) diff this across polls to compute throughput without
    /// needing log parsing. Resets to 0 on every scribe-server restart.
    #[serde(default)]
    pub total_conversions: u64,
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
    /// Live VLM permit count from the server-side shared semaphore
    /// (`Processor::vlm_sem().available_permits()`). Because scribe now
    /// owns exactly one semaphore per process (see rc.286), this is the
    /// truthful "free slot" signal the pool uses to pick the least-loaded
    /// host.
    fn available_slots(&self) -> usize {
        self.vlm_slots_available
    }
    fn total_slots(&self) -> usize {
        self.vlm_slots_total
    }
}

pub struct ScribeClient {
    http: Client,
    server_url: String,
}

/// Default per-request timeout for convert endpoints. Overridden via
/// `ScribeConfig::convert_timeout_secs` / `HS_SCRIBE_CONVERT_TIMEOUT_SECS`.
/// Caps each PDF conversion so a stuck backend can't pin the watcher.
const DEFAULT_CONVERT_TIMEOUT_SECS: u64 = 900;

impl ScribeClient {
    pub fn new(server_url: &str) -> Result<Self> {
        Self::new_with_timeout(
            server_url,
            Duration::from_secs(DEFAULT_CONVERT_TIMEOUT_SECS),
        )
    }

    pub fn new_with_timeout(server_url: &str, convert_timeout: Duration) -> Result<Self> {
        let http = hs_common::http::client_builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(convert_timeout)
            // Detect half-open TCP connections within ~30 s instead of the
            // kernel's default ~2 h. Wi-Fi drops on a laptop scribe used to
            // strand watcher permits for the full 900 s convert_timeout.
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .context("failed to build ScribeClient reqwest Client")?;
        Ok(Self {
            http,
            server_url: server_url.trim_end_matches('/').to_string(),
        })
    }

    /// Create a client with a pre-configured reqwest Client (e.g., with auth headers).
    /// The caller is responsible for setting a request timeout on the provided client.
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
            .timeout(Duration::from_secs(5))
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
    /// Convert a PDF. When `timeout` is `Some`, applies it as the
    /// reqwest per-request timeout and sends the same value in the
    /// `X-Convert-Deadline-Secs` header so the server's
    /// `tokio::time::timeout` wrapper matches. `None` uses the client's
    /// construction-time baseline.
    pub async fn convert(
        &self,
        pdf_bytes: Vec<u8>,
        timeout: Option<Duration>,
    ) -> Result<ConversionResult> {
        let url = format!("{}/scribe", self.server_url);
        let part = reqwest::multipart::Part::bytes(pdf_bytes).file_name("input.pdf");
        let form = reqwest::multipart::Form::new().part("pdf", part);

        let mut req = self.http.post(&url).multipart(form);
        if let Some(d) = timeout {
            req = req
                .timeout(d)
                .header(CONVERT_DEADLINE_HEADER, d.as_secs().to_string());
        }
        let resp = req.send().await.context("Failed to send PDF")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server error {status}: {body}");
        }

        // The non-streaming endpoint returns just markdown bytes — there's
        // no place in the response to carry `per_page_region_classes`.
        // Callers needing region-class info (event_watch's per-page QC)
        // must use convert_with_progress instead. Return an empty class
        // vec here; downstream QC treats it as "not bibliography" and
        // applies the strict default ceiling everywhere — safe.
        let markdown = resp.text().await.context("Failed to read response")?;
        Ok(ConversionResult {
            markdown,
            per_page_region_classes: Vec::new(),
        })
    }

    /// Convert a PDF with streaming progress updates via NDJSON.
    /// Falls back to the plain `/scribe` endpoint if the server doesn't
    /// support streaming (404). `timeout` semantics match
    /// [`ScribeClient::convert`].
    pub async fn convert_with_progress(
        &self,
        pdf_bytes: Vec<u8>,
        timeout: Option<Duration>,
        on_progress: impl Fn(ProgressEvent),
    ) -> Result<ConversionResult> {
        let url = format!("{}/scribe/stream", self.server_url);
        let part = reqwest::multipart::Part::bytes(pdf_bytes.clone()).file_name("input.pdf");
        let form = reqwest::multipart::Form::new().part("pdf", part);

        let mut req = self.http.post(&url).multipart(form);
        if let Some(d) = timeout {
            req = req
                .timeout(d)
                .header(CONVERT_DEADLINE_HEADER, d.as_secs().to_string());
        }
        let mut resp = req.send().await.context("Failed to send PDF")?;

        // Server doesn't support streaming — fall back to plain endpoint.
        // The fallback path's ConversionResult has empty
        // per_page_region_classes; QC treats it as no-bibliography (strict
        // default ceiling everywhere). Old servers can't supply layout
        // metadata; bumping them is the only way to get bibliography-aware
        // thresholds.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            on_progress(ProgressEvent {
                stage: "info".into(),
                page: 0,
                total_pages: 0,
                message: "server does not support progress (update server image)".into(),
            });
            return self.convert(pdf_bytes, timeout).await;
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
                    Ok(StreamLine::Result {
                        markdown,
                        per_page_region_classes,
                    }) => {
                        return Ok(ConversionResult {
                            markdown,
                            per_page_region_classes,
                        })
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TimeoutPolicy {
        TimeoutPolicy::default()
    }

    #[test]
    fn unknown_page_count_uses_fallback() {
        let d = compute_convert_timeout(None, &policy());
        assert_eq!(d.as_secs(), 900);
    }

    #[test]
    fn one_page_clamps_to_floor() {
        let d = compute_convert_timeout(Some(1), &policy());
        assert_eq!(d.as_secs(), 300);
    }

    #[test]
    fn midsize_scales_linearly() {
        let d = compute_convert_timeout(Some(50), &policy());
        // 60 + 50 * 15 = 810
        assert_eq!(d.as_secs(), 810);
    }

    #[test]
    fn huge_book_clamps_to_ceiling() {
        let d = compute_convert_timeout(Some(1000), &policy());
        assert_eq!(d.as_secs(), 3600);
    }

    #[test]
    fn custom_policy_respected() {
        let p = TimeoutPolicy {
            base_secs: 10,
            per_page_secs: 5,
            floor_secs: 30,
            ceiling_secs: 200,
            fallback_secs: 60,
        };
        assert_eq!(compute_convert_timeout(Some(20), &p).as_secs(), 110);
        assert_eq!(compute_convert_timeout(Some(1), &p).as_secs(), 30);
        assert_eq!(compute_convert_timeout(Some(500), &p).as_secs(), 200);
        assert_eq!(compute_convert_timeout(None, &p).as_secs(), 60);
    }
}
