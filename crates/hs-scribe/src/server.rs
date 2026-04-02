use crate::client::StreamLine;
use crate::config::AppConfig;
use crate::pipeline::processor::Processor;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

pub struct ServerState {
    pub processor: Processor,
    pub config: AppConfig,
    pub vlm_sem: Arc<tokio::sync::Semaphore>,
    pub in_flight: Arc<AtomicUsize>,
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
    axum::Json(serde_json::json!({
        "status": "ok", "layout_model": state.processor.has_layout_detector(),
        "table_model": state.processor.has_table_recognizer()
    }))
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

/// Drop guard that decrements in_flight counter when request completes.
struct InFlightGuard(Arc<AtomicUsize>);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn handle_scribe(State(state): State<Arc<ServerState>>, multipart: Multipart) -> Response {
    state.in_flight.fetch_add(1, Ordering::Relaxed);
    let _guard = InFlightGuard(Arc::clone(&state.in_flight));

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
        Ok(md) => (StatusCode::OK, md).into_response(),
        Err(e) => {
            tracing::error!("Processing failed: {e:#}");
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

    state.in_flight.fetch_add(1, Ordering::Relaxed);
    let in_flight = Arc::clone(&state.in_flight);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    let path = tmp.path().to_string_lossy().to_string();
    let vlm_sem = Arc::clone(&state.vlm_sem);

    tokio::spawn(async move {
        let _tmp = tmp; // keep temp file alive for the duration of processing
        let _guard = InFlightGuard(in_flight);

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
                let line = StreamLine::Result { markdown: md };
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
            }
            Err(e) => {
                tracing::error!("Processing failed: {e:#}");
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
