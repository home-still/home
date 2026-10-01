use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hs_common::auth::client::AuthedHttp;
use hs_common::service::protocol::{ReadinessInfo, ServiceClient};
use serde::{Deserialize, Serialize};

use crate::classify::{ConvertFailure, FailureCode};
use crate::config::TimeoutPolicy;

/// HTTP header name carrying the per-request convert deadline. The
/// server reads this and wraps the conversion in
/// `tokio::time::timeout(header_value)`, so client and server agree on
/// how long to wait for a given PDF instead of drifting between
/// independent config defaults. The server clamps it to its configured
/// `max_convert_deadline_secs`; a present value that is not a positive
/// integer is a 400.
pub const CONVERT_DEADLINE_HEADER: &str = "X-Convert-Deadline-Secs";

/// HTTP header carrying the catalog stem the dispatcher is converting.
/// The server has no stem of its own — it sees raw multipart bytes — so
/// without this header the deadline-abort log line on the server
/// identifies *which* PDF wedged only by indirect reasoning. Pass the
/// stem here and the server includes it in `convert deadline exceeded`
/// and `Processing failed` log lines, making `journalctl -u
/// hs-serve-scribe | grep deadline` self-sufficient.
pub const CONVERT_STEM_HEADER: &str = "X-Convert-Stem";

/// Compute the per-request convert timeout from a PDF's page count:
/// `clamp(base + pages * per_page, floor, ceiling)`. The page count is
/// always known — a PDF whose pages cannot be counted is refused before
/// dispatch (`pdf_meta::count_pages`).
pub fn compute_convert_timeout(pages: u32, policy: &TimeoutPolicy) -> Duration {
    let raw = policy
        .base_secs
        .saturating_add(policy.per_page_secs.saturating_mul(u64::from(pages)));
    // `u64::clamp` panics when floor > ceiling; `TimeoutPolicy::validate`
    // rejects that at config load, and `min`/`max` cannot panic anyway.
    Duration::from_secs(raw.max(policy.floor_secs).min(policy.ceiling_secs))
}

// ── NDJSON streaming protocol types ──────────────────────────────

/// Result of a successful PDF→markdown conversion. Carries the assembled
/// markdown plus the per-page list of PP-DocLayout-V3 region class names
/// (index-aligned with `compute_page_offsets` on `markdown`). The class
/// list is empty for pages produced by FullPage mode (no layout
/// detection happens) and by the olmocr converter — both yield an empty
/// `Vec<Vec<String>>`, which downstream QC treats as "not bibliography"
/// (strict default).
///
/// `per_page_diags` carries one [`crate::diag::PageDiagRecord`] per page
/// for the `<output>/<stem>.diag.jsonl` artifact. It is empty when the
/// converter produces no per-page metadata (olmocr) and the consumer
/// simply doesn't write a JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversionResult {
    pub markdown: String,
    #[serde(default)]
    pub per_page_region_classes: Vec<Vec<String>>,
    #[serde(default)]
    pub per_page_diags: Vec<crate::diag::PageDiagRecord>,
}

impl ConversionResult {
    /// Regions the server could not process (0-dim crop, JPEG encode
    /// failure) and left out of the markdown. Any non-zero count means the
    /// markdown has holes; QC refuses to record such a conversion.
    pub fn skipped_regions(&self) -> usize {
        self.per_page_diags
            .iter()
            .map(|d| d.skipped_regions as usize)
            .sum()
    }
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
        #[serde(default)]
        per_page_diags: Vec<crate::diag::PageDiagRecord>,
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

/// `ProgressEvent::stage` of the event the server sends immediately before
/// a conversion's `Error` line. Its `message` is the [`crate::classify::FailureCode`]
/// wire token. It rides the progress channel so the `Error(String)` line
/// keeps its shape: a client that predates typed failures still reads the
/// human message, and a client that knows the token classifies by it.
pub const FAILURE_STAGE: &str = "failed";

/// `HealthResponse::status` when the scribe server is alive but its VLM
/// backend cannot take work (llama-swap unreachable, or the model is
/// not resident and the card has no room to load it). The server pairs
/// this with HTTP 503; `hs status`, the MCP fanout, and the CLI
/// preflight all branch on this exact literal.
pub const BACKEND_UNAVAILABLE: &str = "backend_unavailable";

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
    /// counter, cheap atomic increment. Consumers diff this across polls
    /// to compute throughput without needing log parsing. Resets to 0 on
    /// every scribe-server restart.
    #[serde(default)]
    pub total_conversions: u64,
    /// Whether llama-swap answered the admission probe. `None` on
    /// scribe instances with no llama-swap backend (legacy converter),
    /// where the gate does not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_reachable: Option<bool>,
    /// Whether the configured VLM model is already loaded. When true the
    /// free-VRAM half of the gate is skipped — no cold start is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_model_resident: Option<bool>,
    /// Free VRAM (MiB) at the last probe. `None` on hosts without a
    /// working `nvidia-smi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_vram_free_mb: Option<u64>,
    /// RFC 3339 timestamp of the last admission probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_checked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResponse {
    pub ready: bool,
    pub vlm_slots_total: usize,
    pub vlm_slots_available: usize,
    pub in_flight_conversions: usize,
    /// `"backend_unavailable"` when the zero available slots are the
    /// admission gate refusing, not real saturation. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_status: Option<String>,
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
    fn admits_work(&self) -> bool {
        self.backend_status.as_deref() != Some(BACKEND_UNAVAILABLE)
    }
}

pub struct ScribeClient {
    http: AuthedHttp,
    server_url: String,
    /// Deadline applied to a convert request that does not carry its own
    /// (`convert_with_progress(.., None, ..)`), so no convert request ever
    /// rides on the HTTP client's own default timeout.
    convert_timeout: Duration,
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
            http: AuthedHttp::plain(http),
            server_url: server_url.trim_end_matches('/').to_string(),
            convert_timeout,
        })
    }

    /// Create a client over a pre-built [`AuthedHttp`] (cloud gateway: the
    /// token is attached per request). `convert_timeout` is the deadline of
    /// every convert request that does not set its own.
    pub fn new_with_client(server_url: &str, http: AuthedHttp, convert_timeout: Duration) -> Self {
        Self {
            http,
            server_url: server_url.trim_end_matches('/').to_string(),
            convert_timeout,
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
        let status = resp.status();
        if !status.is_success() {
            // A 404 here is a mis-routed gateway path or a binary that
            // predates /readiness; either way the host is not one the
            // pool can load-balance onto, and saying "ready" would hide it.
            anyhow::bail!(
                "GET {url} answered {status}: {} is not serving /readiness \
                 (wrong gateway route, or a binary that needs `hs upgrade`)",
                self.server_url
            );
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
    /// Convert a PDF with streaming progress updates via NDJSON. The
    /// request always carries an explicit deadline — `timeout`, or the
    /// client's `convert_timeout` when `None` — applied as the reqwest
    /// per-request timeout and sent in the `X-Convert-Deadline-Secs` header
    /// so the server's `tokio::time::timeout` wrapper matches. The bytes
    /// are shared, not copied: a caller that tries several backends hands
    /// each attempt a refcount of the same `Bytes`.
    ///
    /// A failure the server typed (`FailureCode` on the wire) comes back as
    /// a [`ConvertFailure`] at the root of the error, so
    /// [`crate::classify::classify`] decides from the code; any other
    /// failure is untyped and therefore transient.
    ///
    /// `/scribe/stream` is the ONE convert path. A 404 means the server
    /// predates streaming — that's a deploy-discipline error (`hs
    /// upgrade` the host), not something to paper over with a degraded
    /// non-streaming request whose empty region-class list silently
    /// weakens QC.
    pub async fn convert_with_progress(
        &self,
        pdf_bytes: impl Into<bytes::Bytes>,
        timeout: Option<Duration>,
        stem: Option<&str>,
        on_progress: impl Fn(ProgressEvent),
    ) -> Result<ConversionResult> {
        let url = format!("{}/scribe/stream", self.server_url);
        let pdf_bytes: bytes::Bytes = pdf_bytes.into();
        let len = pdf_bytes.len() as u64;
        let part =
            reqwest::multipart::Part::stream_with_length(reqwest::Body::from(pdf_bytes), len)
                .file_name("input.pdf");
        let form = reqwest::multipart::Form::new().part("pdf", part);

        let deadline = timeout.unwrap_or(self.convert_timeout);
        let mut req = self
            .http
            .post(&url)
            .multipart(form)
            .timeout(deadline)
            .header(CONVERT_DEADLINE_HEADER, deadline.as_secs().to_string());
        if let Some(s) = stem {
            req = req.header(CONVERT_STEM_HEADER, s);
        }
        let resp = req.send().await.context("Failed to send PDF")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "scribe server {} has no /scribe/stream endpoint — binary predates \
                 streaming; run `hs upgrade` on that host",
                self.server_url
            );
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let message = format!("Server error {status}: {body}");
            // A refusal the server typed (the 415 content gate) has the
            // wire token, and nothing else, as its body.
            return Err(match FailureCode::from_wire(body.trim()) {
                Some(code) => ConvertFailure::err(code, message),
                None => anyhow::anyhow!(message),
            });
        }

        // Same wire format as `StreamLine` above; the one shared NDJSON
        // reader in hs-common fails on a malformed line instead of skipping
        // it. The typed failure code arrives as a `FAILURE_STAGE` progress
        // event ahead of the `Error` line and is held back from the caller.
        let failure = std::sync::Mutex::new(None::<FailureCode>);
        let outcome = hs_common::service::protocol::read_ndjson_stream::<
            ProgressEvent,
            ConversionResult,
        >(resp, |event| {
            if event.stage == FAILURE_STAGE {
                if let Some(code) = FailureCode::from_wire(&event.message) {
                    if let Ok(mut slot) = failure.lock() {
                        *slot = Some(code);
                    }
                }
                return;
            }
            on_progress(event);
        })
        .await;
        match outcome {
            Ok(result) => Ok(result),
            Err(e) => {
                let code = failure.lock().ok().and_then(|slot| *slot);
                Err(match code {
                    Some(code) => ConvertFailure::err(code, format!("{e:#}")),
                    None => e,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TimeoutPolicy {
        TimeoutPolicy::default()
    }

    #[test]
    fn one_page_clamps_to_floor() {
        let d = compute_convert_timeout(1, &policy());
        assert_eq!(d.as_secs(), 300);
    }

    #[test]
    fn midsize_scales_linearly() {
        let d = compute_convert_timeout(50, &policy());
        // 60 + 50 * 15 = 810
        assert_eq!(d.as_secs(), 810);
    }

    #[test]
    fn huge_book_clamps_to_ceiling() {
        let d = compute_convert_timeout(1000, &policy());
        assert_eq!(d.as_secs(), 3600);
        assert_eq!(compute_convert_timeout(u32::MAX, &policy()).as_secs(), 3600);
    }

    #[test]
    fn custom_policy_respected() {
        let p = TimeoutPolicy {
            base_secs: 10,
            per_page_secs: 5,
            floor_secs: 30,
            ceiling_secs: 200,
        };
        assert_eq!(compute_convert_timeout(20, &p).as_secs(), 110);
        assert_eq!(compute_convert_timeout(1, &p).as_secs(), 30);
        assert_eq!(compute_convert_timeout(500, &p).as_secs(), 200);
    }

    #[test]
    fn an_inverted_policy_cannot_panic_the_dispatcher() {
        // Config load rejects floor > ceiling (config::tests); this pins
        // that even an unvalidated policy degrades to the ceiling instead of
        // the `u64::clamp` panic the dispatch path used to have.
        let p = TimeoutPolicy {
            base_secs: 0,
            per_page_secs: 1,
            floor_secs: 500,
            ceiling_secs: 100,
        };
        assert_eq!(compute_convert_timeout(10, &p).as_secs(), 100);
    }

    /// The server serializes `StreamLine`; the client reads the same bytes
    /// through hs-common's generic reader. The two must stay wire-identical.
    #[test]
    fn server_stream_lines_parse_as_the_shared_stream_line() {
        use hs_common::service::protocol::StreamLine as Shared;

        let progress = serde_json::to_string(&StreamLine::Progress(ProgressEvent {
            stage: "ocr".into(),
            page: 2,
            total_pages: 9,
            message: "page 2".into(),
        }))
        .unwrap();
        let result = serde_json::to_string(&StreamLine::Result {
            markdown: "# Müller".into(),
            per_page_region_classes: vec![vec!["text".into()]],
            per_page_diags: Vec::new(),
        })
        .unwrap();
        let error = serde_json::to_string(&StreamLine::Error("boom".into())).unwrap();

        let p: Shared<ProgressEvent, ConversionResult> = serde_json::from_str(&progress).unwrap();
        assert!(matches!(p, Shared::Progress(e) if e.page == 2 && e.total_pages == 9));
        let r: Shared<ProgressEvent, ConversionResult> = serde_json::from_str(&result).unwrap();
        match r {
            Shared::Result(c) => {
                assert_eq!(c.markdown, "# Müller");
                assert_eq!(c.per_page_region_classes, vec![vec!["text".to_string()]]);
            }
            other => panic!("expected result, got {other:?}"),
        }
        let e: Shared<ProgressEvent, ConversionResult> = serde_json::from_str(&error).unwrap();
        assert!(matches!(e, Shared::Error(m) if m == "boom"));
    }

    /// Answer every connection with `response` after reading its request.
    /// Returns the base URL and a log of the request heads received.
    async fn canned_server(
        response: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (response, seen) = (response.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut head = String::new();
                    // Read until the request head is complete, then drain
                    // what is already in flight so the client can finish
                    // writing before the reply (and close) arrive.
                    while !head.contains("\r\n\r\n") {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        head.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                    let _ = tokio::time::timeout(Duration::from_millis(200), async {
                        while sock.read(&mut buf).await.unwrap_or(0) > 0 {}
                    })
                    .await;
                    seen.lock().unwrap().push(head);
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (url, log)
    }

    fn http(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn a_readiness_404_is_an_error_not_a_synthetic_always_ready() {
        let (url, _) = canned_server(http("404 Not Found", "text/plain", "no route")).await;
        let client = ScribeClient::new(&url).unwrap();
        let err = client.readiness().await.unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }

    #[tokio::test]
    async fn a_415_with_a_failure_token_body_is_typed_and_unknown_bodies_are_not() {
        use crate::classify::{classify, FailureClass};
        let (url, _) = canned_server(http(
            "415 Unsupported Media Type",
            "text/plain",
            "unsupported_content_type:html",
        ))
        .await;
        let err = ScribeClient::new(&url)
            .unwrap()
            .convert_with_progress(b"%PDF-1.4".to_vec(), None, Some("x"), |_| {})
            .await
            .unwrap_err();
        assert_eq!(
            classify(&err),
            FailureClass::Permanent("unsupported_content_type:html")
        );

        // A body that merely mentions a verdict is not one.
        let (url, _) = canned_server(http(
            "500 Internal Server Error",
            "text/plain",
            "paywall FormatError unsupported_content_type:html",
        ))
        .await;
        let err = ScribeClient::new(&url)
            .unwrap()
            .convert_with_progress(b"%PDF-1.4".to_vec(), None, None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(classify(&err), FailureClass::Transient);
    }

    #[tokio::test]
    async fn the_failure_code_riding_the_progress_channel_types_the_error_and_is_not_shown_as_progress(
    ) {
        use crate::classify::{classify, FailureClass};
        let body = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&StreamLine::Progress(ProgressEvent {
                stage: "vlm".into(),
                page: 1,
                total_pages: 2,
                message: "ocr".into()
            }))
            .unwrap(),
            serde_json::to_string(&StreamLine::Progress(ProgressEvent {
                stage: FAILURE_STAGE.into(),
                page: 0,
                total_pages: 0,
                message: FailureCode::VlmRepetitionLoop.wire().into()
            }))
            .unwrap(),
            serde_json::to_string(&StreamLine::Error("loop on page 2".into())).unwrap(),
        );
        let (url, log) = canned_server(http("200 OK", "text/x-ndjson", &body)).await;
        let shown = std::sync::Mutex::new(Vec::new());
        let err = ScribeClient::new(&url)
            .unwrap()
            .convert_with_progress(
                b"%PDF-1.4".to_vec(),
                Some(Duration::from_secs(42)),
                Some("a-stem"),
                |e| shown.lock().unwrap().push(e.stage),
            )
            .await
            .unwrap_err();
        assert_eq!(
            classify(&err),
            FailureClass::Escalate("vlm_repetition_loop")
        );
        assert!(format!("{err:#}").contains("loop on page 2"), "{err:#}");
        assert_eq!(*shown.lock().unwrap(), vec!["vlm".to_string()]);

        // Every convert request carries its own deadline and the stem.
        let head = log.lock().unwrap()[0].to_ascii_lowercase();
        assert!(head.contains("x-convert-deadline-secs: 42"), "{head}");
        assert!(head.contains("x-convert-stem: a-stem"), "{head}");
    }

    #[tokio::test]
    async fn a_convert_without_an_explicit_timeout_still_sends_the_clients_deadline() {
        let (url, log) = canned_server(http("200 OK", "text/x-ndjson", "")).await;
        let client = ScribeClient::new_with_timeout(&url, Duration::from_secs(777)).unwrap();
        let _ = client
            .convert_with_progress(b"%PDF-1.4".to_vec(), None, None, |_| {})
            .await;
        let head = log.lock().unwrap()[0].to_ascii_lowercase();
        assert!(head.contains("x-convert-deadline-secs: 777"), "{head}");
    }
}
