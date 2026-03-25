use crate::config::AppConfig;
use crate::pipeline::processor::Processor;
use axum::{
    extract::{DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub struct ServerState {
    pub processor: Processor,
    pub config: AppConfig,
}

pub fn app(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/scribe", post(handle_scribe))
        .route("/health", get(handle_health))
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

async fn handle_info(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "layout": state.processor.has_layout_detector(),
            "tables": state.processor.has_table_recognizer(),
        }
    }))
}

async fn handle_scribe(
    State(state): State<Arc<ServerState>>,
    mut multipart: Multipart,
) -> Response {
    let mut pdf_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("pdf") {
            match field.bytes().await {
                Ok(b) => pdf_bytes = Some(b.to_vec()),
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("{e}")).into_response();
                }
            }
        }
    }

    let pdf_bytes = match pdf_bytes {
        Some(b) => b,
        None => {
            return (StatusCode::BAD_REQUEST, "Missing 'pdf' field").into_response();
        }
    };

    let tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
        }
    };

    if let Err(e) = std::fs::write(tmp.path(), &pdf_bytes) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
    }

    let path = tmp.path().to_str().unwrap_or_default();
    match state.processor.process_pdf(path).await {
        Ok(md) => (StatusCode::OK, md).into_response(),
        Err(e) => {
            tracing::error!("Processing failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}
