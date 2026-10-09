use crate::backend_probe::{self, BackendState};
use crate::classify::{failure_code, ConvertFailure, FailureCode};
use crate::client::{
    HealthResponse, ProgressEvent, StreamLine, BACKEND_UNAVAILABLE, CONVERT_DEADLINE_HEADER,
    CONVERT_STEM_HEADER, FAILURE_STAGE,
};
use crate::config::{AppConfig, ConverterMode};
use crate::ocr::RepetitionLoopError;
use crate::pdf_meta::{check_header, HEADER_PROBE_BYTES, MAX_PDF_BYTES};
use crate::pipeline::processor::Processor;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use hs_common::auth::backend::BackendToken;
use hs_common::service::inflight::InFlightGuard;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_stream::wrappers::ReceiverStream;

/// Largest request body the server reads: a maximal PDF plus the multipart
/// framing around it.
pub const MAX_UPLOAD_BODY_BYTES: usize = MAX_PDF_BYTES + 64 * 1024;

/// Why a request's `X-Convert-Deadline-Secs` was refused.
#[derive(Debug, PartialEq, Eq)]
struct BadDeadline(String);

/// Resolve the per-request convert deadline. A caller-supplied
/// `X-Convert-Deadline-Secs` header wins; otherwise fall back to the
/// server's configured default. The subscriber sends this header so
/// scaled deadlines stay in sync between client and server — without
/// it, a 500-page book that needs 3600s would still be killed at the
/// server's 900s default.
///
/// A header that is present but not a positive integer is an error (a
/// zero deadline would abort every conversion at once, and a value we cannot
/// read is a client bug, not a request for the default). A value above
/// `max_secs` is clamped to it: the operator's ceiling on how long one
/// request may hold a converter slot. The `bool` is "came from the header".
fn resolve_deadline(
    header: Option<&str>,
    default_secs: u64,
    max_secs: u64,
) -> Result<(Duration, bool), BadDeadline> {
    let Some(raw) = header else {
        return Ok((Duration::from_secs(default_secs), false));
    };
    let secs: u64 = raw.trim().parse().map_err(|_| {
        BadDeadline(format!(
            "{CONVERT_DEADLINE_HEADER} must be a positive integer number of seconds, got {raw:?}"
        ))
    })?;
    if secs == 0 {
        return Err(BadDeadline(format!(
            "{CONVERT_DEADLINE_HEADER} must be at least 1 second"
        )));
    }
    Ok((Duration::from_secs(secs.min(max_secs)), true))
}

/// Read the catalog stem the dispatcher is converting from request
/// headers. Returns `"<unknown>"` when absent so deadline-abort and
/// processing-failed logs always carry a value — older clients that
/// don't send the header still get something printable, and the
/// canonical token makes "header missing" easy to grep.
fn resolve_stem(headers: &HeaderMap) -> &str {
    headers
        .get(CONVERT_STEM_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<unknown>")
}

/// How long a backend verdict stays fresh. `pick_server` polls
/// `/readiness` every 500 ms per queued handler, so an uncached probe
/// would shell out to `nvidia-smi` several times a second under load.
const BACKEND_PROBE_TTL: Duration = Duration::from_secs(5);

/// What converts the PDFs this server accepts. The per-region ONNX
/// pipeline exists only in `Legacy` mode: an olmocr host never loads the
/// layout and table models (which sit on the GPU the VLM backend needs).
pub enum ConverterBackend {
    Legacy(Box<Processor>),
    Olmocr,
}

pub struct ServerState {
    pub converter: ConverterBackend,
    pub config: AppConfig,
    pub in_flight: Arc<AtomicUsize>,
    /// Unix millis of the most recent successful conversion. `0` = never.
    /// Lock-free reads serve `/health` probes without blocking writers.
    pub last_conversion_ms: Arc<AtomicU64>,
    /// Monotonic count of successful conversions since startup. Consumers
    /// diff this across polls for throughput measurement. Lock-free atomic
    /// increment on success.
    pub total_conversions: Arc<AtomicU64>,
    /// Last VLM-backend admission verdict and when it was taken.
    /// `None` until the first probe.
    pub backend_state: Arc<tokio::sync::Mutex<Option<(std::time::Instant, BackendState)>>>,
    /// One permit per conversion the server will run at once
    /// (`vlm_concurrency`). A request takes a permit before its upload is
    /// read and holds it until the conversion ends; with none free it is
    /// refused with 503. In olmocr mode this is the converter semaphore
    /// that caps concurrent `olmocr` subprocesses and that `/readiness`
    /// reports.
    pub admission: Arc<tokio::sync::Semaphore>,
}

impl ServerState {
    /// Validate the config and build exactly what its converter needs. Fails
    /// (the server must not start) when the Legacy pipeline cannot be built.
    pub fn new(config: AppConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let converter = match config.converter {
            ConverterMode::Legacy => {
                ConverterBackend::Legacy(Box::new(Processor::new(config.clone())?))
            }
            ConverterMode::Olmocr => ConverterBackend::Olmocr,
        };
        let admission = Arc::new(tokio::sync::Semaphore::new(config.vlm_concurrency));
        Ok(Self {
            converter,
            config,
            in_flight: Arc::new(AtomicUsize::new(0)),
            last_conversion_ms: Arc::new(AtomicU64::new(0)),
            total_conversions: Arc::new(AtomicU64::new(0)),
            backend_state: Arc::new(tokio::sync::Mutex::new(None)),
            admission,
        })
    }

    fn has_layout_detector(&self) -> bool {
        matches!(&self.converter, ConverterBackend::Legacy(p) if p.has_layout_detector())
    }

    fn has_table_recognizer(&self) -> bool {
        matches!(&self.converter, ConverterBackend::Legacy(p) if p.has_table_recognizer())
    }

    /// Why the layout / table models are not loaded (`None` when they are).
    fn models_reason(&self) -> Option<String> {
        match &self.converter {
            ConverterBackend::Legacy(p) => p.layout_model_reason().map(str::to_string),
            ConverterBackend::Olmocr => Some("not used (converter is olmocr)".to_string()),
        }
    }

    /// `(total, available)` conversion slots as `/readiness` reports them.
    ///
    /// Legacy: the shared VLM-call semaphore (the real contended resource —
    /// regions of every conversion queue on it), capped by the admission
    /// semaphore so a host that cannot admit another upload does not
    /// advertise a free slot. Olmocr: the admission semaphore, which is held
    /// for the whole run of the `olmocr` subprocess.
    fn slots(&self) -> (usize, usize) {
        let admission = self.admission.available_permits();
        match &self.converter {
            ConverterBackend::Legacy(p) => (
                p.effective_vlm_concurrency(),
                p.vlm_sem().available_permits().min(admission),
            ),
            ConverterBackend::Olmocr => (self.config.vlm_concurrency, admission),
        }
    }
}

/// Return the cached backend verdict, re-probing when it is missing or
/// older than [`BACKEND_PROBE_TTL`]. Logs once per verdict *transition*
/// — a per-probe log would emit twice a second per queued handler.
///
/// `None` means "no llama-swap backend to admit against": the legacy
/// converter drives Ollama / a bare OpenAI-compatible server directly
/// and has no `/running` endpoint, so there is nothing to probe and
/// nothing to refuse. Reporting a fabricated `reachable: false` there
/// would readiness-exclude every Apple Silicon pool member.
async fn cached_backend_state(state: &ServerState) -> Option<BackendState> {
    if state.config.converter != ConverterMode::Olmocr {
        return None;
    }
    let mut slot = state.backend_state.lock().await;
    if let Some((at, cached)) = slot.as_ref() {
        if at.elapsed() < BACKEND_PROBE_TTL {
            return Some(cached.clone());
        }
    }
    let fresh = backend_probe::probe(
        &state.config.olmocr_endpoint,
        &state.config.olmocr_model,
        state.config.vram_headroom_mb,
    )
    .await;
    let previous = slot.as_ref().map(|(_, s)| s.admits());
    if previous != Some(fresh.admits()) {
        if fresh.admits() {
            tracing::info!(
                free_vram_mb = ?fresh.free_vram_mb,
                model_resident = fresh.model_resident,
                "vlm backend available again"
            );
        } else {
            let holders = hs_common::gpu::compute_apps_summary_async().await;
            tracing::warn!(
                free_vram_mb = ?fresh.free_vram_mb,
                reachable = fresh.reachable,
                headroom_mb = state.config.vram_headroom_mb,
                holders = %holders,
                "vlm backend unavailable — refusing dispatch"
            );
        }
    }
    *slot = Some((std::time::Instant::now(), fresh.clone()));
    Some(fresh)
}

fn record_success(last_slot: &AtomicU64, total: &AtomicU64, md: &str) {
    if md.trim().is_empty() {
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms > 0 {
        last_slot.store(now_ms, Ordering::Relaxed);
    }
    total.fetch_add(1, Ordering::Relaxed);
}

fn format_last_conv(slot: &AtomicU64) -> Option<String> {
    let ms = slot.load(Ordering::Relaxed);
    if ms == 0 {
        return None;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// The router. This is the endpoint that parses hostile PDFs and drives the
/// GPU, so it is closed by default: every request needs
/// `Authorization: Bearer <HS_BACKEND_TOKEN>` — including paths no route
/// serves — except the two probes in [`OPEN_PROBES`].
pub fn app(state: Arc<ServerState>, token: BackendToken) -> Router {
    app_with_body_limit(state, token, MAX_UPLOAD_BODY_BYTES)
}

/// [`app`] with an explicit request-body limit (the production value is
/// [`MAX_UPLOAD_BODY_BYTES`]).
pub fn app_with_body_limit(
    state: Arc<ServerState>,
    token: BackendToken,
    max_body_bytes: usize,
) -> Router {
    require_token_on_all_but_probes(
        Router::new()
            .route("/scribe/stream", post(handle_scribe_stream))
            .route("/health", get(handle_health))
            .route("/readiness", get(handle_readiness))
            .route("/info", get(handle_info)),
        token,
    )
    .layer(DefaultBodyLimit::max(max_body_bytes))
    .with_state(state)
    // Outermost: a handler panic is a 500 and the server keeps serving.
    .layer(axum::middleware::from_fn(
        hs_common::panic_guard::http::catch_panic,
    ))
}

/// `GET` paths served without credentials: the liveness/readiness probes
/// that pools, the gateway and `hs status` poll. Everything else — any route
/// added later included — is behind the token.
pub const OPEN_PROBES: [&str; 2] = ["/health", "/readiness"];

/// Wrap `router` so that every request but a `GET` of an [`OPEN_PROBES`]
/// path must carry the backend token. Fail-closed: a route added to the
/// router is protected without anyone remembering to protect it.
fn require_token_on_all_but_probes<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    token: BackendToken,
) -> Router<S> {
    router.layer(middleware::from_fn_with_state(token, require_token))
}

/// The backend secret the server requires, from `lookup` (the process
/// environment in production). Unset or unusable is an error naming
/// `HS_BACKEND_TOKEN` (never its value): the server refuses to start.
pub fn backend_token(
    lookup: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> anyhow::Result<BackendToken> {
    BackendToken::from_lookup(lookup).map_err(|e| {
        anyhow::anyhow!(
            "hs-scribe-server requires a backend token: {e:#}. Put HS_BACKEND_TOKEN \
             (>= 32 visible ASCII bytes, e.g. `openssl rand -hex 32`, the same value as the \
             gateway and every client host) in ~/.home-still/secrets.env"
        )
    })
}

async fn require_token(
    State(token): State<BackendToken>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let open =
        request.method() == axum::http::Method::GET && OPEN_PROBES.contains(&request.uri().path());
    if open {
        return next.run(request).await;
    }
    match token.check_authorization(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(why) => (
            StatusCode::UNAUTHORIZED,
            [
                (header::WWW_AUTHENTICATE, "Bearer realm=\"hs-scribe\""),
                (header::CONTENT_TYPE, "application/json"),
            ],
            serde_json::json!({ "error": format!("unauthorized: {why}") }).to_string(),
        )
            .into_response(),
    }
}

/// `status` is `"ok"` only when the VLM backend can actually take work.
/// A refusing gate returns 503 with the same body so an operator sees
/// the free-VRAM number that caused it — `hs status` and the MCP
/// fanout key off `status`, and `ScribeClient::health` parses the body
/// regardless of status code.
async fn handle_health(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let info = hs_common::gpu::query_gpu_info_async().await;
    let backend = cached_backend_state(&state).await;
    let pdfium_fault = crate::pdfium::healthy().err();
    let admits = backend.as_ref().is_none_or(|b| b.admits()) && pdfium_fault.is_none();
    let models_reason = state.models_reason();
    let body = HealthResponse {
        status: if pdfium_fault.is_some() {
            "pdfium_fault"
        } else if admits {
            "ok"
        } else {
            BACKEND_UNAVAILABLE
        }
        .into(),
        layout_model: state.has_layout_detector(),
        table_model: state.has_table_recognizer(),
        layout_model_reason: models_reason.clone(),
        table_model_reason: models_reason,
        version: env!("HS_VERSION").into(),
        gpu_name: info.name,
        gpu_utilization_pct: info.utilization_pct,
        gpu_memory_used_mb: info.memory_used_mb,
        last_conversion_at: format_last_conv(&state.last_conversion_ms),
        total_conversions: state.total_conversions.load(Ordering::Relaxed),
        backend_reachable: backend.as_ref().map(|b| b.reachable),
        backend_model_resident: backend.as_ref().map(|b| b.model_resident),
        backend_vram_free_mb: backend.as_ref().and_then(|b| b.free_vram_mb),
        backend_checked_at: backend.as_ref().map(|b| b.checked_at.clone()),
    };
    if admits {
        (StatusCode::OK, axum::Json(body))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body))
    }
}

async fn handle_readiness(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    // Report the EFFECTIVE capacity. The pool load-balancer relies on
    // these numbers being truthful.
    let (total, free) = state.slots();
    let admits = cached_backend_state(&state)
        .await
        .is_none_or(|b| b.admits())
        && crate::pdfium::healthy().is_ok();
    // Zero available slots is what `ServicePool::try_pick_once` already
    // treats as ineligible, so a closed gate parks the dispatcher
    // instead of feeding a backend that can only time out.
    let available = if admits { free } else { 0 };
    let in_flight = state.in_flight.load(Ordering::Relaxed);
    let mut body = serde_json::json!({
        "ready": available > 0,
        "vlm_slots_total": total,
        "vlm_slots_available": available,
        "in_flight_conversions": in_flight,
    });
    if !admits {
        body["backend_status"] = serde_json::Value::String(BACKEND_UNAVAILABLE.into());
    }
    axum::Json(body)
}

async fn handle_info(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "layout": state.has_layout_detector(),
            "tables": state.has_table_recognizer(),
        }
    }))
}

/// A refused upload, as the response to send.
fn upload_refusal(status: StatusCode, body: impl Into<String>) -> Response {
    (status, body.into()).into_response()
}

/// The refusal for a multipart read error. `MultipartError::status` carries
/// the truth: 413 when the body limit was crossed, 400 for a malformed
/// body — never "Missing 'pdf' field".
fn multipart_refusal(e: axum::extract::multipart::MultipartError) -> Response {
    upload_refusal(e.status(), e.body_text())
}

/// Stream the `pdf` field of a multipart upload into a temp file, once.
///
/// The body never sits in memory: each chunk goes to disk as it arrives
/// (a 256 MiB PDF used to be buffered, copied with `to_vec`, and written
/// out again). The first [`HEADER_PROBE_BYTES`] are held just long enough to
/// apply the `%PDF` gate, so a body that is not a PDF is refused (415, with
/// the failure code as the body) before anything else is written. The temp
/// file keeps a `.pdf` suffix because the olmocr CLI picks inputs by it, and
/// is deleted when the returned handle drops.
#[allow(clippy::result_large_err)]
async fn receive_pdf(mut multipart: Multipart) -> Result<tempfile::NamedTempFile, Response> {
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => {
                return Err(upload_refusal(
                    StatusCode::BAD_REQUEST,
                    "Missing 'pdf' field",
                ))
            }
            Err(e) => return Err(multipart_refusal(e)),
        };
        if field.name() != Some("pdf") {
            continue;
        }

        let io_fault = |what: &str, e: std::io::Error| {
            tracing::error!(error = %e, "upload spool: {what}");
            upload_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{what}: {e}"))
        };
        let tmp = tempfile::Builder::new()
            .prefix("hs-scribe-upload-")
            .suffix(".pdf")
            .tempfile()
            .map_err(|e| io_fault("creating the upload file", e))?;
        let mut file = tokio::fs::File::from_std(
            tmp.reopen()
                .map_err(|e| io_fault("opening the upload file", e))?,
        );

        let refuse_header = |failure: ConvertFailure| {
            tracing::warn!(
                reason = failure.code().wire(),
                "rejecting non-PDF body at the /scribe/stream gate"
            );
            upload_refusal(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                failure.code().wire().to_string(),
            )
        };

        let mut head: Vec<u8> = Vec::with_capacity(HEADER_PROBE_BYTES);
        let mut head_checked = false;
        let mut total: usize = 0;
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => return Err(multipart_refusal(e)),
            };
            total += chunk.len();
            if total > MAX_PDF_BYTES {
                return Err(upload_refusal(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("PDF exceeds the {MAX_PDF_BYTES}-byte limit"),
                ));
            }
            if head_checked {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| io_fault("writing the upload", e))?;
                continue;
            }
            head.extend_from_slice(&chunk);
            if head.len() >= HEADER_PROBE_BYTES {
                check_header(&head).map_err(refuse_header)?;
                file.write_all(&head)
                    .await
                    .map_err(|e| io_fault("writing the upload", e))?;
                head_checked = true;
                head = Vec::new();
            }
        }
        if !head_checked {
            // The whole body is shorter than the probe window.
            check_header(&head).map_err(refuse_header)?;
            file.write_all(&head)
                .await
                .map_err(|e| io_fault("writing the upload", e))?;
        }
        file.flush()
            .await
            .map_err(|e| io_fault("flushing the upload", e))?;
        return Ok(tmp);
    }
}

/// The failure code to put on the wire for a failed conversion: the code
/// the failing stage attached, or the one implied by the streaming
/// repetition detector's abort. `None` for everything else, which clients
/// treat as transient.
fn wire_failure_code(err: &anyhow::Error) -> Option<FailureCode> {
    failure_code(err).or_else(|| {
        err.chain()
            .any(|e| e.is::<RepetitionLoopError>())
            .then_some(FailureCode::VlmRepetitionLoop)
    })
}

fn ndjson(line: &StreamLine) -> Option<String> {
    serde_json::to_string(line)
        .ok()
        .map(|json| format!("{json}\n"))
}

/// Emit a failed conversion: the typed code as a `FAILURE_STAGE` progress
/// event (when the failure has one), then the human message as the
/// `Error` line the stream has always ended with.
async fn send_failure(
    tx: &tokio::sync::mpsc::Sender<Result<String, std::io::Error>>,
    message: String,
    code: Option<FailureCode>,
) {
    if let Some(code) = code {
        if let Some(line) = ndjson(&StreamLine::Progress(ProgressEvent {
            stage: FAILURE_STAGE.into(),
            page: 0,
            total_pages: 0,
            message: code.wire().to_string(),
        })) {
            let _ = tx.send(Ok(line)).await;
        }
    }
    if let Some(line) = ndjson(&StreamLine::Error(message)) {
        let _ = tx.send(Ok(line)).await;
    }
}

type ConvertOutcome = (String, Vec<Vec<String>>, Vec<crate::diag::PageDiagRecord>);

/// Run the configured converter over the spooled PDF.
async fn convert_pdf(
    state: &ServerState,
    path: &std::path::Path,
    on_progress: impl Fn(ProgressEvent) + Send + Sync + 'static,
) -> anyhow::Result<ConvertOutcome> {
    match &state.converter {
        // Legacy streams per-page progress events natively.
        ConverterBackend::Legacy(processor) => processor
            .process_pdf_with_progress(&path.to_string_lossy(), on_progress)
            .await
            .map(|r| (r.markdown, r.per_page_region_classes, r.per_page_diags)),
        // olmocr produces the markdown as one bundle, so the stream emits a
        // single Result line at the end (with empty per-page metadata —
        // olmocr doesn't surface per-page region classes or diags). Its
        // page count comes from the file: the tally olmocr prints is only
        // meaningful against it.
        ConverterBackend::Olmocr => {
            let pages = crate::pdf_meta::count_pages_in_file(path).await?;
            crate::converter::olmocr_subprocess::convert(path, pages, &state.config)
                .await
                .map(|md| (md, Vec::new(), Vec::new()))
        }
    }
}

/// Run a conversion so that a panic in it is a typed, permanent failure on
/// the wire instead of a dropped connection the client retries.
async fn guarded_conversion<T>(
    stem: &str,
    conversion: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match hs_common::panic_guard::catch_panic(conversion).await {
        Ok(result) => result,
        Err(panic) => {
            tracing::error!(
                route = "/scribe/stream",
                stem = %stem,
                panic = %panic,
                "conversion task PANICKED — failing this document permanently"
            );
            Err(ConvertFailure::err(
                FailureCode::ConversionPanicked,
                format!("the scribe server panicked converting this document: {panic}"),
            ))
        }
    }
}

async fn handle_scribe_stream(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    // A bad deadline header is refused before a byte of the upload is read.
    let (deadline, from_header) = match resolve_deadline(
        headers
            .get(CONVERT_DEADLINE_HEADER)
            .and_then(|v| v.to_str().ok()),
        state.config.convert_deadline_secs,
        state.config.max_convert_deadline_secs,
    ) {
        Ok(resolved) => resolved,
        Err(BadDeadline(why)) => return upload_refusal(StatusCode::BAD_REQUEST, why),
    };
    if headers.contains_key(CONVERT_DEADLINE_HEADER) && !from_header {
        // Present but not valid UTF-8: `to_str` failed above.
        return upload_refusal(
            StatusCode::BAD_REQUEST,
            format!("{CONVERT_DEADLINE_HEADER} is not valid text"),
        );
    }
    let stem = resolve_stem(&headers).to_string();

    // Admission comes before the upload is read: a server at capacity must
    // not first spool 256 MiB it cannot use.
    let permit = match Arc::clone(&state.admission).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::warn!(stem = %stem, "converter at capacity — refusing upload");
            let mut resp = upload_refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "converter at capacity; retry later",
            );
            resp.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
            return resp;
        }
    };
    let in_flight_guard = InFlightGuard::new(&state.in_flight);

    let tmp = match receive_pdf(multipart).await {
        Ok(tmp) => tmp,
        Err(resp) => return resp,
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);

    tokio::spawn(async move {
        let _tmp = tmp; // keep the spooled PDF alive for the duration of processing
        let _guard = in_flight_guard;
        let _permit = permit;

        let tx_progress = tx.clone();
        let on_progress = move |event: ProgressEvent| {
            if let Some(line) = ndjson(&StreamLine::Progress(event)) {
                let _ = tx_progress.try_send(Ok(line));
            }
        };

        // The spawned task is outside every middleware: a panic here would
        // end the task, drop `tx` and leave the client an untyped
        // "connection closed" it retries. Catch it, say so, and type it.
        let convert_fut = guarded_conversion(&stem, convert_pdf(&state, _tmp.path(), on_progress));
        // The response body (the receiver) is dropped when the client goes
        // away; nobody can read this conversion's result any more, so stop
        // it here and free the slot — an olmocr subprocess is killed on drop
        // and a Legacy conversion stops at its next await — instead of
        // running it to the deadline for no reader.
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(deadline, convert_fut) => outcome,
            () = tx.closed() => {
                tracing::warn!(
                    stem = %stem,
                    "client disconnected mid-conversion — aborting; slot released"
                );
                return;
            }
        };
        match outcome {
            Ok(Ok((markdown, per_page_region_classes, per_page_diags))) => {
                record_success(
                    &state.last_conversion_ms,
                    &state.total_conversions,
                    &markdown,
                );
                let line = StreamLine::Result {
                    markdown,
                    per_page_region_classes,
                    per_page_diags,
                };
                if let Some(line) = ndjson(&line) {
                    let _ = tx.send(Ok(line)).await;
                }
            }
            Ok(Err(e)) => {
                tracing::error!(stem = %stem, "Processing failed: {e:#}");
                tracing::debug!("Full error chain: {e:?}");
                send_failure(&tx, format!("{e:#}"), wire_failure_code(&e)).await;
            }
            Err(_elapsed) => {
                // tokio::time::timeout fired → inner future chain dropped → every
                // in-flight VLM request aborts, stage-1 spawn_blocking exits on
                // the next send to a closed channel, VLM permit released.
                tracing::error!(
                    stem = %stem,
                    deadline_secs = deadline.as_secs(),
                    from_header,
                    "convert deadline exceeded — aborting stream; slot released"
                );
                send_failure(
                    &tx,
                    format!("convert deadline ({}s) exceeded", deadline.as_secs()),
                    None,
                )
                .await;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "text/x-ndjson")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_success_sets_recent_unix_millis_and_increments_total() {
        let slot = AtomicU64::new(0);
        let total = AtomicU64::new(0);
        record_success(&slot, &total, "# Some markdown\n\nbody");
        let stored = slot.load(Ordering::Relaxed);
        assert!(stored > 0, "timestamp should be set");
        assert_eq!(total.load(Ordering::Relaxed), 1, "total should increment");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            now.saturating_sub(stored) < 5_000,
            "stored ts {stored} should be within 5s of now {now}"
        );
    }

    #[test]
    fn record_success_skips_empty_markdown() {
        let slot = AtomicU64::new(0);
        let total = AtomicU64::new(0);
        record_success(&slot, &total, "   \n\n   ");
        assert_eq!(slot.load(Ordering::Relaxed), 0);
        assert_eq!(
            total.load(Ordering::Relaxed),
            0,
            "empty md shouldn't bump total"
        );
    }

    #[test]
    fn format_last_conv_returns_none_for_unset() {
        let slot = AtomicU64::new(0);
        assert!(format_last_conv(&slot).is_none());
    }

    #[test]
    fn format_last_conv_emits_rfc3339() {
        let slot = AtomicU64::new(1_700_000_000_000); // 2023-11-14T22:13:20Z
        let s = format_last_conv(&slot).expect("set");
        assert!(s.starts_with("2023-11-14T"), "got: {s}");
        assert!(s.ends_with('Z'), "got: {s}");
    }

    #[test]
    fn deadline_defaults_when_the_header_is_absent() {
        assert_eq!(
            resolve_deadline(None, 900, 7200),
            Ok((Duration::from_secs(900), false))
        );
    }

    #[test]
    fn deadline_header_wins_up_to_the_server_ceiling() {
        assert_eq!(
            resolve_deadline(Some("3600"), 900, 7200),
            Ok((Duration::from_secs(3600), true))
        );
        assert_eq!(
            resolve_deadline(Some(" 120 "), 900, 7200),
            Ok((Duration::from_secs(120), true))
        );
        // Above the ceiling: clamped, not honored and not refused.
        assert_eq!(
            resolve_deadline(Some("86400"), 900, 7200),
            Ok((Duration::from_secs(7200), true))
        );
        assert_eq!(
            resolve_deadline(Some("18446744073709551615"), 900, 7200),
            Ok((Duration::from_secs(7200), true))
        );
    }

    #[test]
    fn a_deadline_header_that_is_not_a_positive_integer_is_refused_not_ignored() {
        for bad in [
            "0",
            "-5",
            "",
            "abc",
            "1.5",
            "1e3",
            "99999999999999999999999",
        ] {
            assert!(
                resolve_deadline(Some(bad), 900, 7200).is_err(),
                "{bad:?} must not silently become the default"
            );
        }
    }

    #[tokio::test]
    async fn a_panic_in_the_conversion_task_is_a_typed_permanent_failure() {
        let err = guarded_conversion::<()>("doc", async { panic!("index out of bounds") })
            .await
            .unwrap_err();
        let code = wire_failure_code(&err);
        assert_eq!(code, Some(FailureCode::ConversionPanicked), "{err:#}");
        assert!(matches!(
            crate::classify::classify(&err),
            crate::classify::FailureClass::Permanent(_)
        ));
        // A normal result passes through untouched.
        assert_eq!(guarded_conversion("doc", async { Ok(3) }).await.unwrap(), 3);
    }

    #[test]
    fn typed_failures_go_on_the_wire_and_untyped_ones_do_not() {
        let typed =
            ConvertFailure::err(FailureCode::PdfParseError, "broken").context("converting upload");
        assert_eq!(wire_failure_code(&typed), Some(FailureCode::PdfParseError));

        let looped = anyhow::Error::new(RepetitionLoopError {
            reason: crate::ocr::LoopReason::Bigram,
            partial_output: String::new(),
            bytes_at_abort: 10,
        })
        .context("full-page VLM failed on page 3");
        assert_eq!(
            wire_failure_code(&looped),
            Some(FailureCode::VlmRepetitionLoop)
        );

        assert_eq!(
            wire_failure_code(&anyhow::anyhow!("connection refused")),
            None
        );
    }

    // ── the real router over loopback, olmocr mode with a stand-in CLI ──

    #[cfg(unix)]
    mod http {
        use super::*;
        use crate::pdf_meta::tests::{pdf_with_pages, skip_without_pdfium};
        use std::os::unix::fs::PermissionsExt;

        pub(super) const TOKEN: &str = "scribe-test-backend-token-0123456789abcdef";

        pub(super) fn token() -> BackendToken {
            BackendToken::new(TOKEN).unwrap()
        }

        /// A client that sends the backend token, as every in-repo client does.
        pub(super) fn authed_client() -> reqwest::Client {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {TOKEN}").parse().unwrap(),
            );
            hs_common::http::client_builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .default_headers(headers)
                .build()
                .unwrap()
        }

        struct Rig {
            base: String,
            dir: tempfile::TempDir,
        }

        impl Rig {
            /// The stand-in `olmocr` waits for `<dir>/release` to exist before
            /// it writes its markdown and tally, so a test can hold a
            /// conversion in flight.
            async fn start(vlm_concurrency: usize, body_limit: usize) -> Rig {
                let dir = tempfile::tempdir().unwrap();
                let release = dir.path().join("release");
                let script = dir.path().join("olmocr.sh");
                let md = "Converted text that is long enough to be indexable. ".repeat(4);
                std::fs::write(
                    &script,
                    format!(
                        "#!/bin/sh\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n\
                         mkdir -p \"$1/markdown\" && printf '# T\\n\\n{md}' > \"$1/markdown/o.md\"\n\
                         echo 'Completed pages: 2' >&2; echo 'Failed pages: 0' >&2\n",
                        release.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                // Stand-in llama-swap: the admission gate probes `<endpoint>/running`,
                // so the rig must not depend on whatever backend the host runs.
                let swap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let swap_endpoint = format!("http://{}/v1", swap.local_addr().unwrap());
                tokio::spawn(async move {
                    let running = axum::Router::new().route(
                        "/running",
                        axum::routing::get(|| async {
                            axum::Json(serde_json::json!({ "running": [] }))
                        }),
                    );
                    axum::serve(swap, running).await
                });
                let config = AppConfig {
                    converter: ConverterMode::Olmocr,
                    olmocr_bin: script.to_string_lossy().into_owned(),
                    olmocr_endpoint: swap_endpoint,
                    // Independent of how much VRAM other tenants of the
                    // host hold right now.
                    vram_headroom_mb: 0,
                    vlm_concurrency,
                    ..AppConfig::default()
                };
                let state = Arc::new(ServerState::new(config).unwrap());
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                tokio::spawn(async move {
                    axum::serve(listener, app_with_body_limit(state, token(), body_limit)).await
                });
                Rig { base, dir }
            }

            fn release(&self) {
                std::fs::write(self.dir.path().join("release"), b"").unwrap();
            }

            async fn post(&self, field: &str, body: Vec<u8>) -> reqwest::Response {
                let form = reqwest::multipart::Form::new().part(
                    field.to_string(),
                    reqwest::multipart::Part::bytes(body).file_name("in.pdf"),
                );
                authed_client()
                    .post(format!("{}/scribe/stream", self.base))
                    .multipart(form)
                    .send()
                    .await
                    .unwrap()
            }

            async fn readiness(&self) -> serde_json::Value {
                reqwest::get(format!("{}/readiness", self.base))
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap()
            }
        }

        #[tokio::test]
        async fn a_body_over_the_limit_is_413_not_a_missing_field() {
            let rig = Rig::start(1, 4096).await;
            let mut oversized = pdf_with_pages(2);
            oversized.resize(64 * 1024, b'\n'); // trailing bytes after %%EOF
            let resp = rig.post("pdf", oversized).await;
            assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
            let body = resp.text().await.unwrap();
            assert!(!body.contains("Missing 'pdf' field"), "{body}");
        }

        #[tokio::test]
        async fn bodies_that_are_not_pdfs_are_415_with_the_failure_code_as_body() {
            let rig = Rig::start(1, MAX_UPLOAD_BODY_BYTES).await;
            let resp = rig
                .post("pdf", b"<!DOCTYPE html><html>log in</html>".to_vec())
                .await;
            assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(resp.text().await.unwrap(), "unsupported_content_type:html");
            let resp = rig.post("pdf", vec![0u8, 1, 2, 3, 4, 5]).await;
            assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(
                resp.text().await.unwrap(),
                "unsupported_content_type:binary"
            );
            // Large enough to cross the header-probe window.
            let mut big = b"<html>".to_vec();
            big.resize(HEADER_PROBE_BYTES * 3, b' ');
            let resp = rig.post("pdf", big).await;
            assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            // Nothing was admitted for any of them.
            assert_eq!(rig.readiness().await["vlm_slots_available"], 1);
        }

        #[tokio::test]
        async fn a_request_without_the_pdf_field_is_400() {
            let rig = Rig::start(1, MAX_UPLOAD_BODY_BYTES).await;
            let resp = rig.post("not_pdf", pdf_with_pages(1)).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert!(resp.text().await.unwrap().contains("Missing 'pdf' field"));
        }

        #[tokio::test]
        async fn a_server_at_capacity_refuses_with_503_and_readiness_tracks_the_slot() {
            skip_without_pdfium!("a_server_at_capacity_refuses_with_503");
            let rig = Rig::start(1, MAX_UPLOAD_BODY_BYTES).await;
            let idle = rig.readiness().await;
            assert_eq!(idle["vlm_slots_total"], 1);
            assert_eq!(idle["vlm_slots_available"], 1);
            assert_eq!(idle["ready"], true);

            // First conversion: admitted, held inside the stand-in CLI.
            let first = {
                let (base, body) = (rig.base.clone(), pdf_with_pages(2));
                tokio::spawn(async move {
                    let form = reqwest::multipart::Form::new().part(
                        "pdf",
                        reqwest::multipart::Part::bytes(body).file_name("in.pdf"),
                    );
                    authed_client()
                        .post(format!("{base}/scribe/stream"))
                        .multipart(form)
                        .send()
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap()
                })
            };
            let mut busy = None;
            for _ in 0..200 {
                let r = rig.readiness().await;
                if r["vlm_slots_available"] == 0 {
                    busy = Some(r);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let busy = busy.expect("the in-flight conversion must take the slot");
            assert_eq!(busy["ready"], false);
            assert_eq!(busy["in_flight_conversions"], 1);

            // Second request: refused before its upload is read.
            let refused = rig.post("pdf", pdf_with_pages(2)).await;
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(refused.headers().get(header::RETRY_AFTER).unwrap(), "5");

            // Release: the first completes and the slot comes back.
            rig.release();
            let stream = first.await.unwrap();
            assert!(stream.contains("\"result\""), "{stream}");
            let mut recovered = false;
            for _ in 0..100 {
                let r = rig.readiness().await;
                if r["vlm_slots_available"] == 1 && r["in_flight_conversions"] == 0 {
                    assert_eq!(r["ready"], true);
                    recovered = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(recovered, "the slot must be released after the conversion");
        }

        // ── backend token (the RA-24/26 mechanism, review F4) ───────────

        /// A server over a state that never converts anything: olmocr mode
        /// with an unreachable backend, so no probe leaves loopback.
        async fn bare_server() -> String {
            let config = AppConfig {
                converter: ConverterMode::Olmocr,
                olmocr_endpoint: "http://127.0.0.1:1/v1".into(),
                vlm_concurrency: 1,
                ..AppConfig::default()
            };
            let state = Arc::new(ServerState::new(config).unwrap());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app(state, token())).await });
            base
        }

        fn bearer_client(authorization: Option<&str>) -> reqwest::Client {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Some(value) = authorization {
                headers.insert(reqwest::header::AUTHORIZATION, value.parse().unwrap());
            }
            hs_common::http::client_builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .default_headers(headers)
                .build()
                .unwrap()
        }

        /// Every (method, path) a caller can try: both served routes, the
        /// probes under a wrong method, and paths no route serves.
        fn probes_of_the_surface() -> Vec<(&'static str, &'static str)> {
            vec![
                ("POST", "/scribe/stream"),
                ("GET", "/scribe/stream"),
                ("DELETE", "/scribe/stream"),
                ("GET", "/info"),
                ("POST", "/info"),
                ("POST", "/health"),
                ("POST", "/readiness"),
                ("GET", "/health/"),
                ("GET", "/nothing-serves-this"),
            ]
        }

        async fn send(client: &reqwest::Client, method: &str, url: String) -> reqwest::Response {
            client
                .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url)
                .send()
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn every_request_but_a_probe_needs_the_token() {
            let base = bare_server().await;
            let wrong = format!("Bearer {}", "wrong-token-".repeat(4));
            for authorization in [
                None,
                Some(wrong.as_str()),
                Some("Basic dXNlcjpwYXNz"),
                Some("Bearer"),
            ] {
                let client = bearer_client(authorization);
                for (method, path) in probes_of_the_surface() {
                    let resp = send(&client, method, format!("{base}{path}")).await;
                    assert_eq!(
                        resp.status(),
                        StatusCode::UNAUTHORIZED,
                        "{method} {path} with {authorization:?}"
                    );
                    assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));
                    let text = resp.text().await.unwrap();
                    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert!(
                        body["error"].as_str().unwrap().starts_with("unauthorized"),
                        "{text}"
                    );
                    assert!(!text.contains(TOKEN), "the secret must never be echoed");
                }
            }

            // With the token every one of them gets past authentication.
            let authed = authed_client();
            for (method, path) in probes_of_the_surface() {
                let resp = send(&authed, method, format!("{base}{path}")).await;
                assert_ne!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
            }
            assert_eq!(
                send(&authed, "GET", format!("{base}/info")).await.status(),
                StatusCode::OK
            );
        }

        #[tokio::test]
        async fn the_probes_stay_open() {
            let base = bare_server().await;
            let anonymous = bearer_client(None);
            for path in OPEN_PROBES {
                let resp = send(&anonymous, "GET", format!("{base}{path}")).await;
                assert_ne!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
                // 200, or 503 `backend_unavailable` on /health (the olmocr
                // backend here is deliberately unreachable): a verdict.
                assert!(
                    matches!(
                        resp.status(),
                        StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                    ),
                    "{path}: {}",
                    resp.status()
                );
            }
        }

        /// The router is closed by default: a route added later is protected
        /// without anyone remembering to protect it.
        #[tokio::test]
        async fn a_route_added_to_the_router_is_protected_by_default() {
            let router = require_token_on_all_but_probes(
                Router::<()>::new().route("/added-later", get(|| async { "secret work" })),
                token(),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router).await });

            let resp = send(&bearer_client(None), "GET", format!("{base}/added-later")).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert!(!resp.text().await.unwrap().contains("secret work"));
            let resp = send(&authed_client(), "GET", format!("{base}/added-later")).await;
            assert_eq!(resp.text().await.unwrap(), "secret work");
        }

        #[tokio::test]
        async fn an_unauthenticated_upload_is_refused_before_anything_is_admitted() {
            let rig = Rig::start(1, MAX_UPLOAD_BODY_BYTES).await;
            let form = reqwest::multipart::Form::new().part(
                "pdf",
                reqwest::multipart::Part::bytes(b"%PDF-1.4 anything".to_vec()).file_name("in.pdf"),
            );
            let resp = bearer_client(None)
                .post(format!("{}/scribe/stream", rig.base))
                .multipart(form)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let ready = rig.readiness().await;
            assert_eq!(ready["vlm_slots_available"], 1);
            assert_eq!(ready["in_flight_conversions"], 0);
        }

        #[tokio::test]
        async fn the_scribe_client_authenticates_with_the_token_it_is_given() {
            skip_without_pdfium!("the_scribe_client_authenticates_with_the_token_it_is_given");
            let rig = Rig::start(1, MAX_UPLOAD_BODY_BYTES).await;
            rig.release();
            let http = |token: Option<BackendToken>| {
                hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                    hs_common::http::client_builder().build().unwrap(),
                    token,
                )
            };
            let with = crate::client::ScribeClient::new_with_client(
                &rig.base,
                http(Some(token())),
                Duration::from_secs(30),
            );
            let converted = with
                .convert_with_progress(pdf_with_pages(2), None, Some("t"), |_| {})
                .await
                .expect("a client with the token converts");
            assert!(converted.markdown.contains("Converted text"));

            let without = crate::client::ScribeClient::new_with_client(
                &rig.base,
                http(None),
                Duration::from_secs(30),
            );
            let err = without
                .convert_with_progress(pdf_with_pages(2), None, Some("t"), |_| {})
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("401"), "{err:#}");
            // A refusal of the credential says nothing about the document.
            assert_eq!(
                crate::classify::classify(&err),
                crate::classify::FailureClass::Transient
            );
            // Probes, which pools and `hs status` send without a token, work.
            assert!(without.readiness().await.is_ok());
        }

        #[test]
        fn startup_refuses_a_missing_or_short_token_without_echoing_it() {
            let unset = backend_token(|_| Err(std::env::VarError::NotPresent)).unwrap_err();
            assert!(
                format!("{unset:#}").contains("HS_BACKEND_TOKEN"),
                "{unset:#}"
            );
            let short = backend_token(|_| Ok("sekrit-short".into())).unwrap_err();
            let message = format!("{short:#}");
            assert!(
                message.contains("HS_BACKEND_TOKEN") && !message.contains("sekrit-short"),
                "{message}"
            );
            backend_token(|_| Ok(TOKEN.into())).unwrap();
        }
    }
}
