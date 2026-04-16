use crate::client::{HealthResponse, StreamLine};
use crate::config::AppConfig;
use crate::gpu;
use crate::pipeline::processor::Processor;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

pub struct ServerState {
    pub processor: Processor,
    pub config: AppConfig,
    pub vlm_sem: Arc<tokio::sync::Semaphore>,
    pub in_flight: Arc<AtomicUsize>,
    /// Unix millis of the most recent successful conversion. `0` = never.
    /// Lock-free reads serve `/health` probes without blocking writers.
    pub last_conversion_ms: Arc<AtomicU64>,
}

fn record_success(slot: &AtomicU64, md: &str) {
    if md.trim().is_empty() {
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms > 0 {
        slot.store(now_ms, Ordering::Relaxed);
    }
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
    })
}

async fn handle_readiness(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let total = state.config.vlm_concurrency;
    let available = state.vlm_sem.available_permits();
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

async fn handle_scribe(State(state): State<Arc<ServerState>>, multipart: Multipart) -> Response {
    let _guard = InFlightGuard::new(&state.in_flight);

    let pdf_bytes = match extract_pdf(multipart).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let tmp = match write_tmp_pdf(&pdf_bytes) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let path = tmp.path().to_str().unwrap_or_default();
    match state
        .processor
        .process_pdf_with_shared_sem(path, Arc::clone(&state.vlm_sem))
        .await
    {
        Ok(md) => {
            record_success(&state.last_conversion_ms, &md);
            (StatusCode::OK, md).into_response()
        }
        Err(e) => {
            tracing::error!("Processing failed: {e:#}");
            tracing::debug!("Full error chain: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

async fn handle_scribe_stream(
    State(state): State<Arc<ServerState>>,
    multipart: Multipart,
) -> Response {
    let pdf_bytes = match extract_pdf(multipart).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let tmp = match write_tmp_pdf(&pdf_bytes) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let in_flight_guard = InFlightGuard::new(&state.in_flight);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    let path = tmp.path().to_string_lossy().to_string();
    let vlm_sem = Arc::clone(&state.vlm_sem);

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

        match state
            .processor
            .process_pdf_with_progress_and_sem(&path, on_progress, vlm_sem)
            .await
        {
            Ok(md) => {
                record_success(&state.last_conversion_ms, &md);
                let line = StreamLine::Result { markdown: md };
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
            }
            Err(e) => {
                tracing::error!("Processing failed: {e:#}");
                tracing::debug!("Full error chain: {e:?}");
                let line = StreamLine::Error(format!("{e:#}"));
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
    fn record_success_sets_recent_unix_millis() {
        let slot = AtomicU64::new(0);
        record_success(&slot, "# Some markdown\n\nbody");
        let stored = slot.load(Ordering::Relaxed);
        assert!(stored > 0, "timestamp should be set");
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
        record_success(&slot, "   \n\n   ");
        assert_eq!(slot.load(Ordering::Relaxed), 0);
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
