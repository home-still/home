use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hs_common::service::inflight::InFlightGuard;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use crate::client::{
    DistillProgress, DistillStreamLine, HealthResponse, IndexResult, ReadinessResponse,
    SearchFilters, SearchHit, StatusResponse,
};
use crate::config::DistillServerConfig;
use crate::embed::{Embedder, FallbackEmbedder};
use crate::error::DistillError;

pub struct DistillServerState {
    pub embedder: Arc<FallbackEmbedder>,
    pub qdrant: Arc<qdrant_client::Qdrant>,
    pub config: DistillServerConfig,
    pub in_flight: Arc<AtomicUsize>,
    /// Collections we've already verified/created since process start. The
    /// configured default is seeded at startup; non-default names supplied
    /// via per-request `collection` are lazily ensured on first use, then
    /// recorded here so subsequent requests skip the round-trip.
    pub known_collections: Arc<Mutex<HashSet<String>>>,
}

impl DistillServerState {
    /// Pick the collection for this request and ensure the Qdrant collection
    /// exists. Missing `requested` means "use the configured default" — that
    /// path was vetted at startup, so it short-circuits the lazy-create
    /// machinery.
    pub async fn resolve_collection(
        &self,
        requested: Option<&str>,
    ) -> Result<String, DistillError> {
        let name = requested
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.config.collection_name.clone());

        if name == self.config.collection_name {
            return Ok(name);
        }

        let mut known = self.known_collections.lock().await;
        if !known.contains(&name) {
            crate::qdrant::ensure_collection(&self.qdrant, &name, self.embedder.dimension())
                .await?;
            known.insert(name.clone());
        }
        Ok(name)
    }
}

pub fn app(state: Arc<DistillServerState>) -> Router {
    Router::new()
        .route("/distill", post(handle_distill))
        .route("/distill/stream", post(handle_distill_stream))
        .route("/search", post(handle_search))
        .route("/health", get(handle_health))
        .route("/readiness", get(handle_readiness))
        .route("/status", get(handle_status))
        .route("/exists/{doc_id}", get(handle_exists))
        .route("/doc/{doc_id}", axum::routing::delete(handle_delete_doc))
        .route("/docs", get(handle_list_docs))
        .route("/collection/reset", post(handle_reset_collection))
        .route("/scrub-interstitials", post(handle_scrub_interstitials))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

async fn handle_health(State(state): State<Arc<DistillServerState>>) -> impl IntoResponse {
    let qdrant_version = match state.qdrant.health_check().await {
        Ok(r) => r.version,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("qdrant unreachable: {e}"),
            )
                .into_response();
        }
    };

    Json(HealthResponse {
        status: "ok".into(),
        compute_device: state.embedder.device().to_string(),
        collection: state.config.collection_name.clone(),
        version: env!("HS_VERSION").to_string(),
        qdrant_version,
        embed_model: state.config.embedding.model.clone(),
        qdrant_url: state.config.qdrant_url.clone(),
    })
    .into_response()
}

async fn handle_readiness(State(state): State<Arc<DistillServerState>>) -> impl IntoResponse {
    let in_flight = state.in_flight.load(Ordering::Relaxed);
    Json(ReadinessResponse {
        ready: true,
        in_flight,
    })
}

async fn handle_status(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Query(q): axum::extract::Query<CollectionQuery>,
) -> impl IntoResponse {
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let points_count = match crate::qdrant::collection_info(&state.qdrant, &collection).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let documents_count = match crate::qdrant::distinct_doc_count(&state.qdrant, &collection).await
    {
        Ok(d) => d,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
        }
    };
    Json(StatusResponse {
        collection,
        points_count,
        documents_count,
        compute_device: state.embedder.device().to_string(),
        embed_model: state.config.embedding.model.clone(),
    })
    .into_response()
}

async fn handle_delete_doc(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Path(doc_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<CollectionQuery>,
) -> impl IntoResponse {
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    match crate::qdrant::delete_by_doc_id(&state.qdrant, &collection, &doc_id).await {
        Ok(deleted) => {
            Json(serde_json::json!({"doc_id": doc_id, "deleted": deleted})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn handle_list_docs(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Query(q): axum::extract::Query<ListDocsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100_000);
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    match crate::qdrant::list_doc_ids(&state.qdrant, &collection, limit).await {
        Ok(ids) => Json(serde_json::json!({"doc_ids": ids})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn handle_reset_collection(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Query(q): axum::extract::Query<CollectionQuery>,
) -> impl IntoResponse {
    let dimension = state.embedder.dimension();
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    match crate::qdrant::reset_collection(&state.qdrant, &collection, dimension).await {
        Ok(deleted) => Json(serde_json::json!({
            "collection": collection,
            "deleted_points": deleted,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct CollectionQuery {
    collection: Option<String>,
}

#[derive(Deserialize)]
struct ScrubQuery {
    /// When true, scan and report only — do not delete. Default false.
    #[serde(default)]
    dry_run: bool,
    collection: Option<String>,
}

async fn handle_scrub_interstitials(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Query(q): axum::extract::Query<ScrubQuery>,
) -> impl IntoResponse {
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    match crate::qdrant::scrub_interstitial_chunks(&state.qdrant, &collection, q.dry_run).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ListDocsQuery {
    limit: Option<u64>,
    collection: Option<String>,
}

async fn handle_exists(
    State(state): State<Arc<DistillServerState>>,
    axum::extract::Path(doc_id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<CollectionQuery>,
) -> impl IntoResponse {
    let collection = match state.resolve_collection(q.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    match crate::qdrant::doc_exists(&state.qdrant, &collection, &doc_id).await {
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
    /// Optional catalog entry — when callers have already loaded it via
    /// Storage, pass it in so the server doesn't need its own filesystem
    /// copy of the catalog.
    catalog: Option<hs_common::catalog::CatalogEntry>,
    /// Override the target collection. Missing means use the configured
    /// default (`academic_papers`). Non-default names are lazily created on
    /// first use; once created they share the same vector schema and field
    /// indexes as the default.
    #[serde(default)]
    collection: Option<String>,
}

async fn handle_distill(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<IndexRequest>,
) -> Response {
    let _guard = InFlightGuard::new(&state.in_flight);

    let collection = match state.resolve_collection(req.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };

    let path = std::path::Path::new(&req.path);
    match crate::pipeline::index_document(
        path,
        req.content.as_deref(),
        req.catalog.clone(),
        &state.config,
        &collection,
        crate::pipeline::ContentProfile::for_collection(&collection),
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

    let collection = match state.resolve_collection(req.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(16);
    let path = req.path.clone();
    let content = req.content.clone();
    let catalog = req.catalog.clone();

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
            catalog,
            &state.config,
            &collection,
            crate::pipeline::ContentProfile::for_collection(&collection),
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
    /// Override the target collection. Missing means use the configured
    /// default. See [`IndexRequest::collection`] for routing semantics.
    #[serde(default)]
    collection: Option<String>,
}

async fn handle_search(
    State(state): State<Arc<DistillServerState>>,
    Json(req): Json<SearchRequest>,
) -> Response {
    if req.query.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "Search query cannot be empty").into_response();
    }

    let collection = match state.resolve_collection(req.collection.as_deref()).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };

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

    let filter = req.filters.as_ref().and_then(|f| {
        crate::qdrant::build_filter(f.year.as_deref(), f.topic.as_deref(), f.category.as_deref())
    });

    match crate::qdrant::search(&state.qdrant, &collection, query_vector, limit, filter).await {
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
                        authors: payload
                            .get("authors")
                            .and_then(|v| v.as_list())
                            .map(|list| {
                                list.iter()
                                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        year: payload
                            .get("year")
                            .and_then(|v| v.as_integer())
                            .map(|v| v as u64),
                        doi: payload
                            .get("doi")
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
                        category: payload
                            .get("category")
                            .and_then(|v| v.as_str().map(|s| s.to_string())),
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
