use crate::client::{HealthResponse, StreamLine, CONVERT_DEADLINE_HEADER};
use crate::config::AppConfig;
use crate::gpu;
use crate::pipeline::processor::Processor;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

/// Resolve the per-request convert deadline. A caller-supplied
/// `X-Convert-Deadline-Secs` header wins; otherwise fall back to the
/// server's configured default. The subscriber sends this header so
/// scaled deadlines stay in sync between client and server — without
/// it, a 500-page book that needs 3600s would still be killed at the
/// server's 900s default.
fn resolve_deadline(headers: &HeaderMap, fallback_secs: u64) -> (Duration, bool) {
    match headers
        .get(CONVERT_DEADLINE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(secs) => (Duration::from_secs(secs), true),
        None => (Duration::from_secs(fallback_secs), false),
    }
}

pub struct ServerState {
    pub processor: Processor,
    pub config: AppConfig,
    pub in_flight: Arc<AtomicUsize>,
    /// Unix millis of the most recent successful conversion. `0` = never.
    /// Lock-free reads serve `/health` probes without blocking writers.
    pub last_conversion_ms: Arc<AtomicU64>,
    /// Monotonic count of successful conversions since startup. Consumers
    /// diff this across polls for throughput measurement (see `hs scribe
    /// autotune`). Lock-free atomic increment on success.
    pub total_conversions: Arc<AtomicU64>,
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

pub fn app(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/scribe", post(handle_scribe))
        .route("/scribe/stream", post(handle_scribe_stream))
        .route("/health", get(handle_health))
        .route("/readiness", get(handle_readiness))
        .route("/info", get(handle_info))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024)) // 256MB
        .with_state(state)
}

async fn handle_health(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let (gpu_name, gpu_utilization_pct, gpu_memory_used_mb) = gpu::query_gpu_info();
    axum::Json(HealthResponse {
        status: "ok".into(),
        layout_model: state.processor.has_layout_detector(),
        table_model: state.processor.has_table_recognizer(),
        layout_model_reason: state.processor.layout_model_reason().map(str::to_string),
        table_model_reason: state.processor.table_model_reason().map(str::to_string),
        version: env!("HS_VERSION").into(),
        gpu_name,
        gpu_utilization_pct,
        gpu_memory_used_mb,
        last_conversion_at: format_last_conv(&state.last_conversion_ms),
        total_conversions: state.total_conversions.load(Ordering::Relaxed),
    })
}

async fn handle_readiness(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    // Report the EFFECTIVE capacity, not the configured value — rc.295
    // clamps vlm_concurrency to live OLLAMA_NUM_PARALLEL when the config
    // would oversubscribe Ollama. The pool load-balancer relies on this
    // number being truthful.
    let total = state.processor.effective_vlm_concurrency();
    let available = state.processor.vlm_sem().available_permits();
    let in_flight = state.in_flight.load(Ordering::Relaxed);
    axum::Json(serde_json::json!({
        "ready": available > 0,
        "vlm_slots_total": total,
        "vlm_slots_available": available,
        "in_flight_conversions": in_flight,
    }))
}

async fn handle_info(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "layout": state.processor.has_layout_detector(),
            "tables": state.processor.has_table_recognizer(),
        }
    }))
}

/// Extract PDF bytes from a multipart upload.
async fn extract_pdf(mut multipart: Multipart) -> Result<Vec<u8>, Response> {
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("pdf") {
            match field.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => {
                    return Err((StatusCode::BAD_REQUEST, format!("{e}")).into_response());
                }
            }
        }
    }
    Err((StatusCode::BAD_REQUEST, "Missing 'pdf' field").into_response())
}

/// Gate the content-type before dispatching to the VLM. Paywall HTML
/// renamed `.pdf`, truncated downloads, and encrypted binaries will never
/// convert — the VLM would spend GPU time producing garbage or error out
/// deep in the stack. Reject them at the door with HTTP 415 and a precise
/// reason string so the caller (watch-events, MCP) can stamp
/// `conversion_failed` and stop retrying. `%PDF` is the only acceptance
/// criterion: `PDF-1.x` specifies the header exactly.
#[allow(clippy::result_large_err)]
fn verify_pdf_content(bytes: &[u8]) -> Result<(), Response> {
    let head = &bytes[..bytes.len().min(4096)];
    if head.starts_with(b"%PDF") {
        return Ok(());
    }
    let reason = if hs_common::html::looks_like_html(head) {
        "unsupported_content_type:html"
    } else {
        "unsupported_content_type:binary"
    };
    tracing::warn!(
        reason,
        bytes = bytes.len(),
        "rejecting non-PDF body at /scribe gate"
    );
    Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, reason.to_string()).into_response())
}

#[cfg(test)]
mod verify_pdf_tests {
    use super::verify_pdf_content;
    use axum::http::StatusCode;

    fn status_of(resp: &axum::response::Response) -> StatusCode {
        resp.status()
    }

    #[test]
    fn pdf_header_is_accepted() {
        assert!(verify_pdf_content(b"%PDF-1.7\n...").is_ok());
        assert!(verify_pdf_content(b"%PDF-1.4\n%random binary").is_ok());
    }

    #[test]
    fn html_body_is_rejected_with_415_and_html_reason() {
        let err = verify_pdf_content(b"<!DOCTYPE html><html><body>paywall</body></html>")
            .expect_err("HTML must not be accepted");
        assert_eq!(status_of(&err), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn random_bytes_rejected_with_binary_reason() {
        let err = verify_pdf_content(&[0u8, 1, 2, 3, 4, 5, 6, 7]).expect_err("binary must reject");
        assert_eq!(status_of(&err), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn empty_body_rejected() {
        assert!(verify_pdf_content(&[]).is_err());
    }
}

/// Write PDF bytes to a temp file and return the handle (keeps file alive).
#[allow(clippy::result_large_err)]
fn write_tmp_pdf(pdf_bytes: &[u8]) -> Result<tempfile::NamedTempFile, Response> {
    let tmp = tempfile::NamedTempFile::new()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response())?;
    std::fs::write(tmp.path(), pdf_bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response())?;
    Ok(tmp)
}

use hs_common::service::inflight::InFlightGuard;

async fn handle_scribe(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    let _guard = InFlightGuard::new(&state.in_flight);

    let pdf_bytes = match extract_pdf(multipart).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_pdf_content(&pdf_bytes) {
        return resp;
    }

    let tmp = match write_tmp_pdf(&pdf_bytes) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let path = tmp.path().to_str().unwrap_or_default();
    let (deadline, from_header) = resolve_deadline(&headers, state.config.convert_deadline_secs);
    let fut = state.processor.process_pdf(path);
    match tokio::time::timeout(deadline, fut).await {
        Ok(Ok(md)) => {
            record_success(&state.last_conversion_ms, &state.total_conversions, &md);
            (StatusCode::OK, md).into_response()
        }
        Ok(Err(e)) => {
            tracing::error!("Processing failed: {e:#}");
            tracing::debug!("Full error chain: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
        Err(_elapsed) => {
            // tokio::time::timeout fired → inner future chain dropped → every
            // in-flight Ollama request aborts, stage-1 spawn_blocking exits on
            // the next send to a closed channel, VLM permit released.
            tracing::error!(
                deadline_secs = deadline.as_secs(),
                from_header,
                "convert deadline exceeded — aborting; slot released"
            );
            (
                StatusCode::GATEWAY_TIMEOUT,
                format!("convert deadline ({}s) exceeded", deadline.as_secs()),
            )
                .into_response()
        }
    }
}

async fn handle_scribe_stream(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    let pdf_bytes = match extract_pdf(multipart).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_pdf_content(&pdf_bytes) {
        return resp;
    }

    let tmp = match write_tmp_pdf(&pdf_bytes) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let in_flight_guard = InFlightGuard::new(&state.in_flight);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    let path = tmp.path().to_string_lossy().to_string();

    let (deadline, from_header) = resolve_deadline(&headers, state.config.convert_deadline_secs);
    tokio::spawn(async move {
        let _tmp = tmp; // keep temp file alive for the duration of processing
        let _guard = in_flight_guard;

        let tx_progress = tx.clone();
        let on_progress = move |event: crate::client::ProgressEvent| {
            let line = StreamLine::Progress(event);
            if let Ok(json) = serde_json::to_string(&line) {
                let _ = tx_progress.try_send(Ok(format!("{json}\n")));
            }
        };

        let fut = state
            .processor
            .process_pdf_with_progress(&path, on_progress);
        match tokio::time::timeout(deadline, fut).await {
            Ok(Ok(result)) => {
                record_success(
                    &state.last_conversion_ms,
                    &state.total_conversions,
                    &result.markdown,
                );
                let line = StreamLine::Result {
                    markdown: result.markdown,
                    per_page_region_classes: result.per_page_region_classes,
                };
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
            }
            Ok(Err(e)) => {
                tracing::error!("Processing failed: {e:#}");
                tracing::debug!("Full error chain: {e:?}");
                let line = StreamLine::Error(format!("{e:#}"));
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
            }
            Err(_elapsed) => {
                tracing::error!(
                    deadline_secs = deadline.as_secs(),
                    from_header,
                    "convert deadline exceeded — aborting stream; slot released"
                );
                let line = StreamLine::Error(format!(
                    "convert deadline ({}s) exceeded",
                    deadline.as_secs()
                ));
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
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
}
