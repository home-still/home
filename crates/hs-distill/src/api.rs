//! Transport-independent request handling for the distill server.
//!
//! `server` is a thin axum adapter over [`DistillServerState`]; everything
//! that decides what a request means — validation, collection routing,
//! limits, error classification — lives here so it runs (and is tested)
//! without a socket, a GPU, or a Qdrant.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hs_common::service::inflight::InFlightGuard;
use serde::Deserialize;

use crate::client::{
    DistillProgress, HealthResponse, IndexResult, ReadinessResponse, SearchFilters, SearchHit,
    StatusResponse, MAX_DOC_LIST_LIMIT, MAX_SEARCH_LIMIT,
};
use crate::collection::CollectionSpec;
use crate::config::DistillServerConfig;
use crate::embed::{Embedder, EmbedderHealth, MODEL_NAME};
use crate::error::DistillError;
use crate::pipeline::{self, ContentProfile, IndexJob};
use crate::store::{SearchFilter, VectorStore};
use crate::types::ScrubReport;

/// How long `/readiness` waits for Qdrant before reporting not-ready.
const READINESS_QDRANT_TIMEOUT: Duration = Duration::from_secs(2);

/// Hits returned when a search request names no `limit`.
const DEFAULT_SEARCH_LIMIT: u64 = 10;

/// Machine-readable code sent with a 500 caused by a panic in the index
/// task (`x-hs-error-code` header; prefix of the NDJSON error line).
pub const PANIC_CODE: &str = "index_panicked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request is malformed or names something that does not exist
    /// (HTTP 400). Retrying it unchanged cannot succeed.
    BadRequest,
    /// A dependency the server needs is down (HTTP 503).
    Unavailable,
    /// Anything else (HTTP 500).
    Internal,
    /// The indexing code panicked (HTTP 500 + [`PANIC_CODE`]). Deterministic
    /// for the document that triggered it: retrying cannot help.
    Panicked,
}

#[derive(Debug)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::BadRequest,
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<DistillError> for ApiError {
    fn from(e: DistillError) -> Self {
        let kind = match e {
            DistillError::InvalidInput(_) => ErrorKind::BadRequest,
            _ => ErrorKind::Internal,
        };
        Self {
            kind,
            message: e.to_string(),
        }
    }
}

/// Body of `POST /distill` and `POST /distill/stream`.
#[derive(Deserialize)]
pub struct IndexRequest {
    /// Document name. Only its file stem is used (it becomes the `doc_id`
    /// and, with the catalog, the payload's `markdown_path`); the file it
    /// names is never opened.
    pub path: Option<String>,
    /// The markdown to index. Required: the server indexes the text it is
    /// sent and never reads documents from disk.
    pub content: Option<String>,
    /// Catalog entry for the document. Without one the chunks carry no
    /// title/authors/DOI/year.
    pub catalog: Option<hs_common::catalog::CatalogEntry>,
    /// Target collection; must be one the server is configured to serve.
    /// Missing means the default collection.
    #[serde(default)]
    pub collection: Option<String>,
}

/// Body of `POST /search`.
#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Hits wanted (default 10, at most [`MAX_SEARCH_LIMIT`]: larger values
    /// are clamped).
    pub limit: Option<u64>,
    pub filters: Option<SearchFilters>,
    #[serde(default)]
    pub collection: Option<String>,
}

/// A validated index request.
pub struct PreparedIndex {
    doc_id: String,
    path_hint: String,
    content: String,
    catalog: Option<hs_common::catalog::CatalogEntry>,
    collection: String,
}

/// Documents returned by `/docs`.
pub struct DocList {
    pub doc_ids: Vec<String>,
    /// More documents exist than were returned.
    pub truncated: bool,
}

pub struct DistillServerState {
    pub embedder: Arc<dyn Embedder>,
    pub store: Arc<dyn VectorStore>,
    pub config: DistillServerConfig,
    pub in_flight: Arc<AtomicUsize>,
}

impl DistillServerState {
    pub fn new(
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn VectorStore>,
        config: DistillServerConfig,
    ) -> Self {
        Self {
            embedder,
            store,
            config,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The collection a request targets. Missing means the configured
    /// default; any other name must be one of the collections configured
    /// (and verified at startup) — a request can never create or reach an
    /// arbitrary Qdrant collection by naming it.
    pub fn resolve_collection(&self, requested: Option<&str>) -> Result<String, ApiError> {
        let Some(name) = requested else {
            return Ok(self.config.collection_name.clone());
        };
        if self.config.served_collections().any(|c| c == name) {
            return Ok(name.to_string());
        }
        Err(ApiError::bad_request(format!(
            "unknown collection {name:?}; this server serves: {}. Add it to \
             distill_server.collections to serve it",
            self.config
                .served_collections()
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    fn spec(&self) -> CollectionSpec {
        CollectionSpec::new(self.embedder.dimension(), &self.config.hnsw)
    }

    // ── index ──────────────────────────────────────────────────────

    /// Validate an index request before any work starts.
    pub fn prepare_index(&self, req: IndexRequest) -> Result<PreparedIndex, ApiError> {
        let content = req.content.ok_or_else(|| {
            ApiError::bad_request(
                "`content` is required: the server indexes the text it is sent and never reads \
                 documents from disk",
            )
        })?;
        let path_hint = req.path.ok_or_else(|| {
            ApiError::bad_request(
                "`path` is required: the document name whose file stem is the doc_id",
            )
        })?;
        let doc_id = doc_id_from_hint(&path_hint)?;
        let collection = self.resolve_collection(req.collection.as_deref())?;
        Ok(PreparedIndex {
            doc_id,
            path_hint,
            content,
            catalog: req.catalog,
            collection,
        })
    }

    /// Chunk, embed and store a prepared document.
    pub async fn run_index(
        &self,
        job: PreparedIndex,
        on_progress: impl Fn(DistillProgress),
    ) -> Result<IndexResult, ApiError> {
        let _guard = InFlightGuard::new(&self.in_flight);
        let PreparedIndex {
            doc_id,
            path_hint,
            content,
            catalog,
            collection,
        } = job;
        let work = pipeline::index_document(
            IndexJob {
                doc_id: &doc_id,
                markdown_path: &path_hint,
                content: &content,
                catalog,
                collection: &collection,
                profile: ContentProfile::for_collection(&collection),
            },
            &self.config,
            self.embedder.as_ref(),
            self.store.as_ref(),
            on_progress,
        );
        // A panic here would otherwise unwind out of a spawned task (stream
        // route) with no response, or become an untyped 500: surface it as a
        // typed, non-retryable failure and name the document.
        let chunks =
            match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(work)).await {
                Ok(r) => r.map_err(|e| {
                    tracing::error!("Indexing failed: {e}");
                    ApiError::from(e)
                })?,
                Err(_) => {
                    tracing::error!(doc_id = %doc_id, "indexing panicked");
                    return Err(ApiError {
                        kind: ErrorKind::Panicked,
                        message: format!("{PANIC_CODE}: indexing {doc_id} panicked"),
                    });
                }
            };
        Ok(IndexResult {
            doc_id,
            chunks_indexed: chunks,
            embedding_device: self.embedder.device().to_string(),
        })
    }

    // ── search ─────────────────────────────────────────────────────

    pub async fn search(&self, req: SearchRequest) -> Result<Vec<SearchHit>, ApiError> {
        if req.query.trim().is_empty() {
            return Err(ApiError::bad_request("Search query cannot be empty"));
        }
        let collection = self.resolve_collection(req.collection.as_deref())?;
        let filter = SearchFilter::from_request(req.filters.as_ref())?;
        let limit = match req.limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(0) => return Err(ApiError::bad_request("limit must be at least 1")),
            Some(n) => n.min(MAX_SEARCH_LIMIT),
        };

        let embeddings = self.embedder.embed_batch(vec![req.query]).await?;
        let query_vector = embeddings
            .into_iter()
            .next()
            .map(|e| e.dense)
            .ok_or_else(|| DistillError::Embedding("embedder returned no vector".into()))?;

        Ok(self
            .store
            .search(&collection, query_vector, limit, &filter)
            .await?)
    }

    // ── health / readiness / status ────────────────────────────────

    pub async fn health(&self) -> Result<HealthResponse, ApiError> {
        // /health is unauthenticated: it says *that* a dependency is down,
        // never why (the cause is logged). Detail lives behind the token.
        if let EmbedderHealth::Failed(why) = self.embedder.health() {
            tracing::warn!("health: embedder unusable: {why}");
            return Err(ApiError::unavailable("embedder unusable"));
        }
        let qdrant_version = self.store.health().await.map_err(|e| {
            tracing::warn!("health: qdrant unavailable: {e}");
            ApiError::unavailable("qdrant unavailable")
        })?;
        Ok(HealthResponse {
            status: "ok".into(),
            compute_device: self.embedder.device().to_string(),
            collection: self.config.collection_name.clone(),
            version: env!("HS_VERSION").to_string(),
            qdrant_version,
            embed_model: MODEL_NAME.into(),
        })
    }

    /// Ready means this server can index and search right now: the embedder
    /// is healthy and Qdrant answers. `capacity` is the embedder's slot
    /// count; `in_flight` the requests being served.
    pub async fn readiness(&self) -> ReadinessResponse {
        let reason = match self.embedder.health() {
            EmbedderHealth::Failed(why) => {
                tracing::warn!("readiness: embedder unusable: {why}");
                Some("embedder unusable".to_string())
            }
            EmbedderHealth::Healthy => {
                match tokio::time::timeout(READINESS_QDRANT_TIMEOUT, self.store.health()).await {
                    Ok(Ok(_)) => None,
                    Ok(Err(e)) => {
                        tracing::warn!("readiness: qdrant unavailable: {e}");
                        Some("qdrant unavailable".to_string())
                    }
                    Err(_) => Some("qdrant health check timed out".to_string()),
                }
            }
        };
        ReadinessResponse {
            ready: reason.is_none(),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            capacity: self.embedder.slots(),
            reason,
        }
    }

    pub async fn status(&self, collection: Option<&str>) -> Result<StatusResponse, ApiError> {
        let collection = self.resolve_collection(collection)?;
        let points_count = self.store.points_count(&collection).await?;
        let docs = self.store.doc_ids(&collection, MAX_DOC_LIST_LIMIT).await?;
        Ok(StatusResponse {
            collection,
            points_count,
            documents_count: docs.ids.len() as u64,
            documents_count_truncated: docs.truncated,
            compute_device: self.embedder.device().to_string(),
            embed_model: MODEL_NAME.into(),
            qdrant_url: self.config.qdrant_url.clone(),
        })
    }

    // ── per-document and maintenance operations ────────────────────

    pub async fn doc_exists(
        &self,
        doc_id: &str,
        collection: Option<&str>,
    ) -> Result<(bool, u64), ApiError> {
        validate_doc_id(doc_id)?;
        let collection = self.resolve_collection(collection)?;
        let chunks = self.store.doc_chunks(&collection, doc_id).await?;
        Ok((chunks > 0, chunks))
    }

    /// Delete every chunk of `doc_id`; returns how many existed.
    pub async fn delete_doc(
        &self,
        doc_id: &str,
        collection: Option<&str>,
    ) -> Result<u64, ApiError> {
        validate_doc_id(doc_id)?;
        let collection = self.resolve_collection(collection)?;
        let existing = self.store.doc_chunks(&collection, doc_id).await?;
        if existing > 0 {
            self.store
                .delete_chunks_from(&collection, doc_id, 0)
                .await?;
        }
        Ok(existing)
    }

    /// Distinct doc ids. `limit` defaults to, and may not exceed,
    /// [`MAX_DOC_LIST_LIMIT`]; a request above it is rejected rather than
    /// silently truncated, because callers diff this list against storage.
    pub async fn list_docs(
        &self,
        limit: Option<u64>,
        collection: Option<&str>,
    ) -> Result<DocList, ApiError> {
        let limit = match limit {
            None => MAX_DOC_LIST_LIMIT,
            Some(0) => return Err(ApiError::bad_request("limit must be at least 1")),
            Some(n) if n > MAX_DOC_LIST_LIMIT => {
                return Err(ApiError::bad_request(format!(
                    "limit {n} exceeds the maximum of {MAX_DOC_LIST_LIMIT}"
                )))
            }
            Some(n) => n,
        };
        let collection = self.resolve_collection(collection)?;
        let docs = self.store.doc_ids(&collection, limit).await?;
        Ok(DocList {
            doc_ids: docs.ids,
            truncated: docs.truncated,
        })
    }

    /// Drop and recreate a served collection. Destructive; the route is behind
    /// the backend-token middleware (see `server.rs`).
    pub async fn reset_collection(
        &self,
        collection: Option<&str>,
    ) -> Result<(String, u64), ApiError> {
        let collection = self.resolve_collection(collection)?;
        let deleted = self.store.reset(&collection, &self.spec()).await?;
        Ok((collection, deleted))
    }

    /// Submit the configured HNSW parameters to a served collection (see
    /// `VectorStore::enable_hnsw`). Returns immediately; never called at
    /// startup.
    pub async fn enable_hnsw(
        &self,
        collection: Option<&str>,
    ) -> Result<crate::store::HnswEnable, ApiError> {
        let collection = self.resolve_collection(collection)?;
        Ok(self
            .store
            .enable_hnsw(&collection, &self.config.hnsw)
            .await?)
    }

    pub async fn scrub_interstitials(
        &self,
        dry_run: bool,
        collection: Option<&str>,
    ) -> Result<ScrubReport, ApiError> {
        let collection = self.resolve_collection(collection)?;
        Ok(self.store.scrub_interstitials(&collection, dry_run).await?)
    }
}

/// The doc id a document name designates: the file stem of `path`, which
/// must be a usable stem (non-empty, no separators or dot segments). A name
/// that yields none is rejected — the old `"unknown"` fallback merged every
/// such document into one doc id.
pub fn doc_id_from_hint(path: &str) -> Result<String, ApiError> {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ApiError::bad_request(format!("`path` {path:?} has no document name")))?;
    validate_doc_id(stem)?;
    Ok(stem.to_string())
}

fn validate_doc_id(doc_id: &str) -> Result<(), ApiError> {
    hs_common::validate_stem(doc_id)
        .map_err(|e| ApiError::bad_request(format!("invalid doc_id {doc_id:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ReadinessResponse;
    use crate::testutil::{prose, small_chunk_config, FakeEmbedder, FakeStore, Op};
    use hs_common::service::protocol::ReadinessInfo;

    struct Harness {
        state: DistillServerState,
        embedder: Arc<FakeEmbedder>,
        store: Arc<FakeStore>,
    }

    fn harness() -> Harness {
        let embedder = Arc::new(FakeEmbedder::new());
        let store = Arc::new(FakeStore::default());
        let state = DistillServerState::new(embedder.clone(), store.clone(), small_chunk_config());
        Harness {
            state,
            embedder,
            store,
        }
    }

    fn index_req(
        path: Option<&str>,
        content: Option<&str>,
        collection: Option<&str>,
    ) -> IndexRequest {
        IndexRequest {
            path: path.map(str::to_string),
            content: content.map(str::to_string),
            catalog: None,
            collection: collection.map(str::to_string),
        }
    }

    fn search_req(
        query: &str,
        limit: Option<u64>,
        filters: Option<SearchFilters>,
    ) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            limit,
            filters,
            collection: None,
        }
    }

    fn kind<T>(r: Result<T, ApiError>) -> ErrorKind {
        match r {
            Err(e) => e.kind,
            Ok(_) => panic!("expected an error"),
        }
    }

    // ── RA-3: no server-side reads ─────────────────────────────────────

    #[tokio::test]
    async fn a_path_only_request_is_rejected_and_nothing_is_read_or_stored() {
        let h = harness();
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("passwd.md");
        std::fs::write(&secret, "root:x:0:0 ".repeat(100)).unwrap();

        let err = h
            .state
            .prepare_index(index_req(secret.to_str(), None, None))
            .err()
            .expect("path-only must be rejected");
        assert_eq!(err.kind, ErrorKind::BadRequest);
        assert!(err.message.contains("content"), "{err}");

        assert_eq!(h.embedder.calls(), 0, "nothing may be embedded");
        assert_eq!(h.store.ops(), [], "nothing may be stored");
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn index_requests_need_both_a_name_and_content() {
        let h = harness();
        assert_eq!(
            kind(h.state.prepare_index(index_req(None, Some("x"), None))),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.prepare_index(index_req(Some("a.md"), None, None))),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.prepare_index(index_req(None, None, None))),
            ErrorKind::BadRequest
        );
    }

    #[tokio::test]
    async fn content_is_indexed_under_the_stem_of_the_name() {
        let h = harness();
        let job = h
            .state
            .prepare_index(index_req(
                Some("markdown/ab/10.1609_aaai.v38i16.29728.md"),
                Some(&prose(6)),
                None,
            ))
            .unwrap();
        let result = h.state.run_index(job, |_| {}).await.unwrap();
        assert_eq!(result.doc_id, "10.1609_aaai.v38i16.29728");
        assert!(result.chunks_indexed > 0);
        assert_eq!(result.embedding_device, "Cuda");
        assert_eq!(
            h.store
                .stored("academic_papers", "10.1609_aaai.v38i16.29728")
                .len() as u32,
            result.chunks_indexed
        );
        assert_eq!(
            h.state.in_flight.load(Ordering::Relaxed),
            0,
            "guard released"
        );
    }

    #[test]
    fn names_without_a_usable_stem_are_rejected_not_filed_under_unknown() {
        for bad in ["", "/", ".", "..", "a/..", "a\\b", "a\0b"] {
            assert_eq!(
                doc_id_from_hint(bad).expect_err(bad).kind,
                ErrorKind::BadRequest,
                "{bad:?}"
            );
        }
        for (hint, stem) in [
            ("plain", "plain"),
            ("stem.md", "stem"),
            ("markdown/ab/x.y.md", "x.y"),
            ("medical-record.pdf", "medical-record"),
        ] {
            assert_eq!(doc_id_from_hint(hint).unwrap(), stem);
        }
    }

    // ── RA-26: collection allow-list ───────────────────────────────────

    #[test]
    fn only_configured_collections_resolve() {
        let h = harness();
        assert_eq!(h.state.resolve_collection(None).unwrap(), "academic_papers");
        for name in ["academic_papers", "paper_abstracts", "personal_docs"] {
            assert_eq!(h.state.resolve_collection(Some(name)).unwrap(), name);
        }
        for name in [
            "other",
            "",
            "academic_papers ",
            "ACADEMIC_PAPERS",
            "../x",
            "x'; drop",
        ] {
            let e = h.state.resolve_collection(Some(name)).expect_err(name);
            assert_eq!(e.kind, ErrorKind::BadRequest, "{name:?}");
            assert!(e.message.contains("distill_server.collections"), "{e}");
        }
    }

    #[tokio::test]
    async fn every_operation_rejects_an_unknown_collection_without_touching_the_store() {
        let h = harness();
        let c = Some("not_configured");

        assert_eq!(
            kind(h.state.prepare_index(index_req(Some("a.md"), Some("x"), c))),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(
                h.state
                    .search(SearchRequest {
                        collection: c.map(str::to_string),
                        ..search_req("q", None, None)
                    })
                    .await
            ),
            ErrorKind::BadRequest
        );
        assert_eq!(kind(h.state.status(c).await), ErrorKind::BadRequest);
        assert_eq!(
            kind(h.state.doc_exists("d", c).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.delete_doc("d", c).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.list_docs(None, c).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.reset_collection(c).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.scrub_interstitials(true, c).await),
            ErrorKind::BadRequest
        );

        assert_eq!(
            h.store.ops(),
            [],
            "no collection may be created, reset or written"
        );
        assert!(h.store.state.lock().searches.is_empty());
        assert_eq!(h.embedder.calls(), 0);
    }

    #[tokio::test]
    async fn reset_drops_only_the_named_configured_collection() {
        let h = harness();
        h.store.seed("academic_papers", "a", 3);
        h.store.seed("paper_abstracts", "a", 1);
        let (name, deleted) = h
            .state
            .reset_collection(Some("paper_abstracts"))
            .await
            .unwrap();
        assert_eq!((name.as_str(), deleted), ("paper_abstracts", 1));
        assert_eq!(h.store.stored("academic_papers", "a").len(), 3);
        assert_eq!(
            h.store.ops(),
            [Op::Reset {
                collection: "paper_abstracts".into()
            }]
        );
    }

    // ── RA-89: search and listing limits ───────────────────────────────

    #[tokio::test]
    async fn an_unparseable_year_filter_is_a_400_not_an_unfiltered_search() {
        let h = harness();
        let filters = SearchFilters {
            year: Some("last year".into()),
            ..Default::default()
        };
        assert_eq!(
            kind(h.state.search(search_req("q", None, Some(filters))).await),
            ErrorKind::BadRequest
        );
        assert!(
            h.store.state.lock().searches.is_empty(),
            "must not have searched"
        );
        assert_eq!(h.embedder.calls(), 0);
    }

    #[tokio::test]
    async fn search_limit_defaults_and_is_clamped() {
        let h = harness();
        h.state.search(search_req("q", None, None)).await.unwrap();
        h.state
            .search(search_req("q", Some(25), None))
            .await
            .unwrap();
        h.state
            .search(search_req("q", Some(u64::MAX), None))
            .await
            .unwrap();
        let limits: Vec<u64> = h.store.state.lock().searches.iter().map(|s| s.1).collect();
        assert_eq!(limits, [10, 25, MAX_SEARCH_LIMIT]);

        assert_eq!(
            kind(h.state.search(search_req("q", Some(0), None)).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.search(search_req("  ", None, None)).await),
            ErrorKind::BadRequest
        );
    }

    #[tokio::test]
    async fn search_forwards_parsed_filters_and_the_embedded_query() {
        let h = harness();
        let filters = SearchFilters {
            year: Some(">=2020".into()),
            topic: Some("autism".into()),
            category: None,
        };
        h.state
            .search(search_req("find me", Some(5), Some(filters)))
            .await
            .unwrap();
        let st = h.store.state.lock();
        let (collection, limit, filter) = &st.searches[0];
        assert_eq!((collection.as_str(), *limit), ("academic_papers", 5));
        assert_eq!(filter.year, Some(crate::store::YearFilter::Gte(2020)));
        assert_eq!(filter.topic.as_deref(), Some("autism"));
        assert_eq!(h.embedder.texts.lock()[0], ["find me"]);
    }

    #[tokio::test]
    async fn list_docs_limit_is_bounded_and_never_silently_truncated() {
        let h = harness();
        for d in ["a", "b", "c"] {
            h.store.seed("academic_papers", d, 1);
        }
        assert_eq!(
            kind(h.state.list_docs(Some(MAX_DOC_LIST_LIMIT + 1), None).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.list_docs(Some(u64::MAX), None).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(h.state.list_docs(Some(0), None).await),
            ErrorKind::BadRequest
        );

        let all = h.state.list_docs(None, None).await.unwrap();
        assert_eq!(all.doc_ids, ["a", "b", "c"]);
        assert!(!all.truncated);

        let some = h.state.list_docs(Some(2), None).await.unwrap();
        assert_eq!(some.doc_ids.len(), 2);
        assert!(
            some.truncated,
            "the caller must be told the list is partial"
        );
    }

    // ── status / health / readiness (RA-90, RA-93) ─────────────────────

    #[tokio::test]
    async fn status_counts_points_and_documents() {
        let h = harness();
        h.store.seed("academic_papers", "a", 3);
        h.store.seed("academic_papers", "b", 2);
        let s = h.state.status(None).await.unwrap();
        assert_eq!((s.points_count, s.documents_count), (5, 2));
        assert!(!s.documents_count_truncated);
        assert_eq!(s.embed_model, "bge-m3");
        assert_eq!(s.collection, "academic_papers");
    }

    #[tokio::test]
    async fn readiness_reports_capacity_and_real_health() {
        let h = harness();
        let r = h.state.readiness().await;
        assert!(r.ready && r.reason.is_none());
        assert_eq!(r.capacity, 2);
        assert_eq!(r.available_slots(), 2);

        h.state.in_flight.store(5, Ordering::Relaxed);
        let r = h.state.readiness().await;
        assert_eq!(
            (r.in_flight, r.available_slots()),
            (5, 0),
            "over capacity is no slots, not underflow"
        );
        h.state.in_flight.store(0, Ordering::Relaxed);

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Failed("slot 0 poisoned".into());
        let r = h.state.readiness().await;
        assert!(!r.ready);
        assert_eq!(r.available_slots(), 0);
        let reason = r.reason.unwrap();
        assert!(reason.contains("embedder unusable"));
        assert!(
            !reason.contains("poisoned"),
            "internal detail leaked: {reason}"
        );

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Healthy;
        h.store.state.lock().down = true;
        let r = h.state.readiness().await;
        assert!(!r.ready);
        assert!(r.reason.unwrap().contains("qdrant"));
    }

    #[tokio::test]
    async fn health_fails_with_503_semantics_when_a_dependency_is_down() {
        let h = harness();
        let ok = h.state.health().await.unwrap();
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.embed_model, "bge-m3");
        assert_eq!(ok.qdrant_version, "1.0.0");

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Failed("poisoned".into());
        assert_eq!(kind(h.state.health().await), ErrorKind::Unavailable);

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Healthy;
        h.store.state.lock().down = true;
        assert_eq!(kind(h.state.health().await), ErrorKind::Unavailable);
    }

    /// F10/F11: the open probe must not reveal the Qdrant address or the
    /// cause of a failure; the protected `/status` carries the address.
    #[tokio::test]
    async fn the_open_health_probe_leaks_neither_address_nor_cause() {
        let h = harness();
        let ok = serde_json::to_string(&h.state.health().await.unwrap()).unwrap();
        assert!(
            !ok.contains("qdrant_url") && !ok.contains(&h.state.config.qdrant_url),
            "{ok}"
        );

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Failed("slot 0 poisoned".into());
        let err = h.state.health().await.unwrap_err();
        assert!(!err.to_string().contains("poisoned"), "{err}");

        *h.embedder.health.lock() = crate::embed::EmbedderHealth::Healthy;
        h.store.state.lock().down = true;
        let err = h.state.health().await.unwrap_err().to_string();
        assert!(err.contains("qdrant unavailable"), "{err}");
        h.store.state.lock().down = false;

        let status = h.state.status(None).await.unwrap();
        assert_eq!(status.qdrant_url, h.state.config.qdrant_url);
    }

    #[test]
    fn old_servers_readiness_json_still_parses_with_a_single_slot() {
        let old: ReadinessResponse =
            serde_json::from_str(r#"{"ready":true,"in_flight":0}"#).unwrap();
        assert_eq!(old.available_slots(), 1);
        assert!(old.reason.is_none());
    }

    // ── error classification (RA-93) ───────────────────────────────────

    #[test]
    fn bad_input_is_a_400_and_everything_else_is_a_500() {
        assert_eq!(
            ApiError::from(DistillError::InvalidInput("x".into())).kind,
            ErrorKind::BadRequest
        );
        for e in [
            DistillError::Qdrant("x".into()),
            DistillError::Embedding("x".into()),
            DistillError::Metadata("x".into()),
            DistillError::Config("x".into()),
        ] {
            assert_eq!(ApiError::from(e).kind, ErrorKind::Internal);
        }
    }

    #[tokio::test]
    async fn a_store_failure_while_indexing_is_a_500_not_a_400() {
        let h = harness();
        h.store.state.lock().fail_upsert_from_call = Some(0);
        let job = h
            .state
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), None))
            .unwrap();
        assert_eq!(
            kind(h.state.run_index(job, |_| {}).await),
            ErrorKind::Internal
        );
    }

    // ── per-document operations ────────────────────────────────────────

    #[tokio::test]
    async fn delete_reports_how_many_chunks_existed_and_removes_them() {
        let h = harness();
        h.store.seed("academic_papers", "d", 4);
        assert_eq!(h.state.delete_doc("d", None).await.unwrap(), 4);
        assert!(h.store.stored("academic_papers", "d").is_empty());
        assert_eq!(h.state.delete_doc("d", None).await.unwrap(), 0);
        assert_eq!(h.state.doc_exists("d", None).await.unwrap(), (false, 0));
    }

    #[tokio::test]
    async fn doc_ids_that_could_traverse_are_rejected() {
        let h = harness();
        for bad in ["..", ".", "a/b", "a\\b"] {
            assert_eq!(
                kind(h.state.doc_exists(bad, None).await),
                ErrorKind::BadRequest,
                "{bad:?}"
            );
            assert_eq!(
                kind(h.state.delete_doc(bad, None).await),
                ErrorKind::BadRequest,
                "{bad:?}"
            );
        }
        assert_eq!(h.store.ops(), []);
    }

    #[tokio::test]
    async fn a_panic_while_indexing_is_a_typed_failure_and_releases_the_slot() {
        let h = harness();
        *h.embedder.panic.lock() = true;
        let job = h
            .state
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), None))
            .unwrap();
        let err = h
            .state
            .run_index(job, |_| {})
            .await
            .err()
            .expect("must fail");
        assert_eq!(err.kind, ErrorKind::Panicked);
        assert!(
            err.message.starts_with(PANIC_CODE) && err.message.contains("d"),
            "{err}"
        );
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(h.store.ops(), [], "nothing written");
    }
}
