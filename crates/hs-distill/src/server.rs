use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hs_common::service::inflight::InFlightGuard;
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

use crate::client::{
    DistillProgress, DistillStreamLine, HealthResponse, IndexResult, ReadinessResponse,
    SearchFilters, SearchHit, StatusResponse,
};
use crate::config::DistillServerConfig;
use crate::embed::{Embedder, FallbackEmbedder};

pub struct DistillServerState {
    pub embedder: Arc<FallbackEmbedder>,
    pub qdrant: Arc<qdrant_client::Qdrant>,
    pub config: DistillServerConfig,
    pub in_flight: Arc<AtomicUsize>,
}

pub fn app(state: Arc<DistillServerState>) -> Router {
    Router::new()
        .route("/distill", post(handle_distill))
        .route("/distill/stream", post(handle_distill_stream))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .route("/readiness", get(handle_readiness))
        .route("/status", get(handle_status))
        .route("/exists/:doc_id", get(handle_exists))
        .with_state(state)
}

async fn handle_health(State(state): State<Arc<DistillServerState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".into(),
        compute_device: state.embedder.device().to_string(),
        collection: state.config.collection_name.clone(),
    })
}

async fn handle_readiness(State(state): State<Arc<DistillServerState>>) -> impl IntoResponse {
    let in_flight = state.in_flight.load(Ordering::Relaxed);
    Json(ReadinessResponse {
        ready: true,
        in_flight,
    })
}

async fn handle_status(State(state): State<Arc<DistillServerState>>) -> impl IntoResponse {
    let collection = &state.config.collection_name;
    let points = crate::qdrant::collection_info(&state.qdrant, collection).await;
    let docs = crate::qdrant::distinct_doc_count(&state.qdrant, collection).await;

    match points {
        Ok(points_count) => Json(StatusResponse {
            collection: collection.clone(),
            points_count,
            documents_count: docs.unwrap_or(0),
            compute_device: state.embedder.device().to_string(),
        })
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn handle_exists(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Path(doc_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match crate::qdrant::doc_exists(&state.qdrant, &state.config.collection_name, &doc_id).await {
        Ok((exists, chunks)) => {
            Json(serde_json::json!({"exists": exists, "chunks": chunks})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct IndexRequest {
    /// Filename (stem used as doc_id)
    path: String,
    /// If provided, use this content instead of reading from disk
    content: Option<String>,
}

async fn handle_distill(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<IndexRequest>,
) -> Response {
    let _guard = InFlightGuard::new(&state.in_flight);

    let path = std::path::Path::new(&req.path);
    match crate::pipeline::index_document(
        path,
        req.content.as_deref(),
        &state.config,
        state.embedder.as_ref(),
        &state.qdrant,
        |_| {}, // no progress for non-streaming
    )
    .await
    {
        Ok(chunks) => Json(IndexResult {
            doc_id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            chunks_indexed: chunks,
            embedding_device: state.embedder.device().to_string(),
        })
        .into_response(),
        Err(e) => {
            tracing::error!("Indexing failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response()
        }
    }
}

async fn handle_distill_stream(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<IndexRequest>,
) -> Response {
    let guard = InFlightGuard::new(&state.in_flight);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    let path = req.path.clone();
    let content = req.content.clone();

    tokio::spawn(async move {
        let _guard = guard;
        let tx_progress = tx.clone();

        let on_progress = move |event: DistillProgress| {
            let line: DistillStreamLine = hs_common::service::protocol::StreamLine::Progress(event);
            if let Ok(json) = serde_json::to_string(&line) {
                let _ = tx_progress.try_send(Ok(format!("{json}\n")));
            }
        };

        let doc_path = std::path::Path::new(&path);
        match crate::pipeline::index_document(
            doc_path,
            content.as_deref(),
            &state.config,
            state.embedder.as_ref(),
            &state.qdrant,
            on_progress,
        )
        .await
        {
            Ok(chunks) => {
                let result = IndexResult {
                    doc_id: doc_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    chunks_indexed: chunks,
                    embedding_device: state.embedder.device().to_string(),
                };
                let line: DistillStreamLine =
                    hs_common::service::protocol::StreamLine::Result(result);
                if let Ok(json) = serde_json::to_string(&line) {
                    let _ = tx.send(Ok(format!("{json}\n"))).await;
                }
            }
            Err(e) => {
                tracing::error!("Indexing failed: {e}");
                let line: DistillStreamLine =
                    hs_common::service::protocol::StreamLine::Error(format!("{e}"));
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

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    limit: Option<u64>,
    filters: Option<SearchFilters>,
}

async fn handle_search(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<SearchRequest>,
) -> Response {
    // Embed the query
    let query_texts = vec![req.query.clone()];
    let embeddings = match state.embedder.embed_batch(&query_texts).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Embedding failed: {e}"),
            )
                .into_response()
        }
    };

    let query_vector = match embeddings.into_iter().next() {
        Some(e) => e.dense,
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "No embedding produced").into_response()
        }
    };

    let limit = req.limit.unwrap_or(10);

    match crate::qdrant::search(
        &state.qdrant,
        &state.config.collection_name,
        query_vector,
        limit,
    )
    .await
    {
        Ok(results) => {
            let hits: Vec<SearchHit> = results
                .into_iter()
                .filter_map(|point| {
                    let payload = point.payload;
                    Some(SearchHit {
                        doc_id: payload
                            .get("doc_id")?
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                        title: payload
                            .get("title")
                            .and_then(|v| v.as_str().map(|s| s.to_string())),
                        chunk_text: payload
                            .get("chunk_text")?
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                        score: point.score,
                        pdf_path: payload
                            .get("pdf_path")
                            .and_then(|v| v.as_str().map(|s| s.to_string())),
                        line_start: payload
                            .get("line_start")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(0) as usize,
                        line_end: payload
                            .get("line_end")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(0) as usize,
                        page: payload
                            .get("page")
                            .and_then(|v| v.as_integer())
                            .map(|v| v as usize),
                    })
                })
                .collect();

            Json(hits).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Search failed: {e}"),
        )
            .into_response(),
    }
}
