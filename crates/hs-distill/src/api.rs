//! Transport-independent request handling for the distill server.
//!
//! `server` is a thin axum adapter over [`DistillServerState`]; everything
//! that decides what a request means — validation, collection routing,
//! limits, error classification — lives here so it runs (and is tested)
//! without a socket, a GPU, or a myelin.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hs_common::service::inflight::InFlightGuard;
use serde::Deserialize;

use crate::client::{
    DistillProgress, HealthResponse, IndexResult, ReadinessResponse, SearchFilters, SearchHit,
    StatusResponse, MAX_DOC_LIST_LIMIT, MAX_SEARCH_LIMIT,
};
use crate::config::DistillServerConfig;
use crate::embed_device::{ollama_embed_device, DEVICE_CUDA, OLLAMA_EMBED_MODEL};
use crate::error::DistillError;
use crate::pipeline::{self, ContentProfile, IndexJob};
use crate::reference_list;
use crate::store::{DocumentStore, SearchFilter};

/// How long `/readiness` waits for each backend (myelin, ollama) before
/// reporting not-ready.
const READINESS_BACKEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Hits returned when a search request names no `limit`.
const DEFAULT_SEARCH_LIMIT: u64 = 10;

/// Longest search query accepted, in bytes. The query is embedded and then
/// read by the reranker next to every candidate; without a bound a single
/// request could hand them megabytes and stall every document being indexed
/// behind it.
pub const MAX_SEARCH_QUERY_BYTES: usize = 64 * 1024;

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
            DistillError::Backend(_) => ErrorKind::Unavailable,
            _ => ErrorKind::Internal,
        };
        Self {
            kind,
            message: e.to_string(),
        }
    }
}

/// Body of `POST /embedder/release`.
#[derive(Debug, serde::Serialize)]
pub struct EmbedderRelease {
    /// Model slots dropped from the GPU.
    pub released: usize,
}

/// Body of `POST /distill` and `POST /distill/stream`.
#[derive(Deserialize)]
pub struct IndexRequest {
    /// Document name. Only its file stem is used (it becomes the `doc_id`
    /// and, with the catalog, the stored `markdown_path`); the file it
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
    pub store: Arc<dyn DocumentStore>,
    /// HTTP client for ollama, which runs the embedding model behind the
    /// store (`config.ollama_url`).
    pub ollama: reqwest::Client,
    pub config: DistillServerConfig,
    pub in_flight: Arc<AtomicUsize>,
}

impl DistillServerState {
    pub fn new(
        store: Arc<dyn DocumentStore>,
        ollama: reqwest::Client,
        config: DistillServerConfig,
    ) -> Self {
        Self {
            store,
            ollama,
            config,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The collection a request targets. Missing means the configured
    /// default; any other name must be one of the collections configured —
    /// a request can never reach an arbitrary myelin tenant by naming it.
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

    /// Where ollama has the embedding model: `"cuda"`, `"cpu"` or
    /// `"partial"`.
    async fn embedder_device(&self) -> Result<String, DistillError> {
        ollama_embed_device(&self.ollama, &self.config.ollama_url).await
    }

    /// The embedder's device, provided it is fully on the GPU. Embedding
    /// runs on the GPU or not at all: any other placement, and any failure
    /// to find out, is [`ErrorKind::Unavailable`]. The cause is logged, not
    /// returned (the open probes must not reveal it).
    async fn require_gpu_embedder(&self) -> Result<String, ApiError> {
        let device = self.embedder_device().await.map_err(|e| {
            tracing::warn!("embedder unavailable: {e}");
            ApiError::unavailable("embedder unavailable")
        })?;
        if device != DEVICE_CUDA {
            tracing::warn!(%device, "the embedding model is not fully on the GPU");
            return Err(ApiError::unavailable("embedder not on GPU"));
        }
        Ok(device)
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

    /// Chunk and store a prepared document.
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
        // Embedding happens inside the store's write; refuse to start one
        // while the model is not on the GPU.
        let device = self.require_gpu_embedder().await?;
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
            embedding_device: device,
        })
    }

    // ── search ─────────────────────────────────────────────────────

    pub async fn search(&self, req: SearchRequest) -> Result<Vec<SearchHit>, ApiError> {
        if req.query.trim().is_empty() {
            return Err(ApiError::bad_request("Search query cannot be empty"));
        }
        if req.query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(ApiError::bad_request(format!(
                "search query is {} bytes; the maximum is {MAX_SEARCH_QUERY_BYTES}",
                req.query.len()
            )));
        }
        let collection = self.resolve_collection(req.collection.as_deref())?;
        let filter = SearchFilter::from_request(req.filters.as_ref())?;
        let limit = match req.limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(0) => return Err(ApiError::bad_request("limit must be at least 1")),
            Some(n) => n.min(MAX_SEARCH_LIMIT),
        };

        // The store ranks: dense and BM25 candidates fused by rank, then
        // reordered by a cross-encoder.
        let mut hits = self
            .store
            .search(&collection, &req.query, limit, &filter)
            .await?;
        demote_reference_lists(&mut hits, limit);
        Ok(hits)
    }

    // ── health / readiness / status ────────────────────────────────

    pub async fn health(&self) -> Result<HealthResponse, ApiError> {
        // /health is unauthenticated: it says *that* a dependency is down,
        // never why (the cause is logged). Detail lives behind the token.
        let compute_device = self.require_gpu_embedder().await?;
        let backend_version = self.store.health().await.map_err(|e| {
            tracing::warn!("health: myelin unavailable: {e}");
            ApiError::unavailable("myelin unavailable")
        })?;
        Ok(HealthResponse {
            status: "ok".into(),
            compute_device,
            collection: self.config.collection_name.clone(),
            version: env!("HS_VERSION").to_string(),
            backend_version,
            embed_model: OLLAMA_EMBED_MODEL.into(),
            pid: std::process::id(),
        })
    }

    /// Ready means this server can index and search right now: the
    /// embedding model is on the GPU and myelin answers. Writes are
    /// serialized, so `capacity` is 1; `in_flight` is the requests being
    /// served.
    pub async fn readiness(&self) -> ReadinessResponse {
        let reason = self.not_ready_reason().await;
        ReadinessResponse {
            ready: reason.is_none(),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            capacity: 1,
            reason,
        }
    }

    async fn not_ready_reason(&self) -> Option<String> {
        match tokio::time::timeout(READINESS_BACKEND_TIMEOUT, self.require_gpu_embedder()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!("readiness: {e}");
                return Some(e.message);
            }
            Err(_) => return Some("embedder check timed out".to_string()),
        }
        match tokio::time::timeout(READINESS_BACKEND_TIMEOUT, self.store.health()).await {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => {
                tracing::warn!("readiness: myelin unavailable: {e}");
                Some("myelin unavailable".to_string())
            }
            Err(_) => Some("myelin health check timed out".to_string()),
        }
    }

    pub async fn status(&self, collection: Option<&str>) -> Result<StatusResponse, ApiError> {
        let collection = self.resolve_collection(collection)?;
        let inventory = self.store.inventory(&collection, None).await?;
        Ok(StatusResponse {
            collection,
            points_count: inventory.values().sum(),
            documents_count: inventory.len() as u64,
            documents_count_truncated: false,
            compute_device: self.embedder_device().await?,
            embed_model: OLLAMA_EMBED_MODEL.into(),
            myelin_url: self.config.myelin_url.clone(),
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
        let inventory = self.store.inventory(&collection, Some(doc_id)).await?;
        let chunks = inventory.get(doc_id).copied().unwrap_or(0);
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
        Ok(self.store.forget_document(&collection, doc_id, &[]).await?)
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
        let mut doc_ids: Vec<String> = self
            .store
            .inventory(&collection, None)
            .await?
            .into_keys()
            .collect();
        let truncated = doc_ids.len() as u64 > limit;
        doc_ids.truncate(limit as usize);
        Ok(DocList { doc_ids, truncated })
    }

    /// Delete every document of a served collection. Destructive; the route
    /// is behind the backend-token middleware (see `server.rs`). Returns the
    /// collection and how many chunks were deleted.
    pub async fn reset_collection(
        &self,
        collection: Option<&str>,
    ) -> Result<(String, u64), ApiError> {
        let collection = self.resolve_collection(collection)?;
        let inventory = self.store.inventory(&collection, None).await?;
        let mut deleted = 0;
        for doc_id in inventory.keys() {
            deleted += self.store.forget_document(&collection, doc_id, &[]).await?;
        }
        Ok((collection, deleted))
    }
}

/// Move reference-list chunks below the prose chunks, but only past prose
/// that is *competitive with them*.
///
/// A bibliography entry matches a query about a topic perfectly well in
/// a dense embedding — semantically, a citation to work about
/// hippocampal neurogenesis *is* about hippocampal neurogenesis — so the
/// dense channel ranks it at the top. Measured on this corpus, a
/// reference list was the best-ranked hit for "Gazzaniga split brain",
/// and 24 % of the top 3 results across a fixed query set were
/// reference lists.
///
/// The chunks are **demoted, not dropped**: a user asking for a
/// paper's bibliography wants it, so it stays in the result, keeps its
/// score, and is reachable by asking for more. Only its position
/// changes — a prose chunk about the topic now comes first.
///
/// **A reference list is only demoted past prose that outranks it.**
/// Otherwise the demotion is a sort, and a sort can push the *answer*
/// out of the page: measured, demoting every reference list past every
/// prose chunk lost the expected passage for two of the 25 fixed
/// queries (recall 25/25 → 23/25), because for those queries the
/// expected passage was itself a reference list — the one chunk in the
/// corpus that mentions the term. A user asking "what does this corpus
/// say about Dawson and Krebs?" wants that chunk whether or not it is
/// formatted as a bibliography.
///
/// So a reference list moves down only past prose that beats it. If
/// nothing beats it, it keeps the lead: it was the best answer the
/// retrieval found.
///
/// The demotion is applied to the fused list before the page is cut, so
/// prose from below the page can be promoted into it — and the page is
/// cut to the caller's limit *after* the demotion, so the promotion
/// cannot change how many results the caller gets. Measured: demoting a
/// 50-candidate pool and then taking the top 10 drops recall to 23/25 —
/// it pushes good hits out of the window to make room for prose that was
/// never close.
fn demote_reference_lists(hits: &mut Vec<SearchHit>, limit: u64) {
    // A reference list is demoted below prose, but the demotion must
    // never change *which* hits are in the page. Two properties:
    //
    //   * the page keeps exactly the hits it had — the demotion
    //     reorders, it does not add or drop;
    //   * a reference list that outranked every prose chunk keeps its
    //     place, because then it was the best answer the retrieval
    //     found. (Measured: demoting every reference list past every
    //     prose chunk lost the expected passage for two of the 25 fixed
    //     queries, because for those the expected passage was itself a
    //     reference list — the one chunk in the corpus that mentions
    //     the term.)
    //
    // So: the prose keeps its relative order, the reference lists keep
    // theirs, and a reference list moves down only past prose that
    // beats it.
    let mut prose: Vec<SearchHit> = Vec::with_capacity(hits.len());
    let mut references: Vec<SearchHit> = Vec::with_capacity(hits.len());
    for hit in hits.drain(..) {
        if reference_list::is_reference_list(&hit.chunk_text) {
            references.push(hit);
        } else {
            prose.push(hit);
        }
    }
    if references.is_empty() || prose.is_empty() {
        // Nothing to demote past: either every hit is prose, or every
        // hit is a reference list and the user asked for the
        // bibliography.
        let mut merged = prose.into_iter().chain(references).collect::<Vec<_>>();
        merged.truncate(limit as usize);
        *hits = merged;
        return;
    }

    // The prose keeps its fused order and the reference lists keep
    // theirs; the prose is placed first, and the reference lists follow.
    //
    // This is a **stable partition**: the relative order of the hits
    // within each group is the fused order, unchanged. A merge that
    // re-sorted by score would disturb the fused ranking of hits that
    // scored the same, and that costs recall.
    //
    // The prose leads **when there is prose**. A reference list that
    // outranks every prose chunk is not thereby the answer: for "Gazzaniga
    // split brain" the best-ranked hit was a chunk of `doi:` lines
    // scoring 0.500, while the passage that discusses the topic scored
    // 0.250 — and the user asked a question, not for a bibliography.
    // The passage that discusses the topic is the answer.
    //
    // When the page holds **no** prose at all, the reference list is
    // the answer and keeps its place: sometimes the only chunk in the
    // corpus that mentions a term is a citation of it, and there is
    // nothing else to show. (For "Dawson and Krebs" that was the case:
    // the corpus has 137 chunks about a botanist named Dawson and no
    // paper about the Krebs cycle by that name.)
    let mut merged: Vec<SearchHit> = Vec::with_capacity(prose.len() + references.len());
    merged.extend(prose);
    merged.extend(references);
    // The page is unchanged in size: the demotion reorders, and the
    // caller's limit still decides how many results there are.
    merged.truncate(limit as usize);
    *hits = merged;
}

/// The doc id a document name designates: the file stem of `path`, which
/// must be a usable stem (non-empty, no separators or dot segments). A name
/// that yields none is rejected — the old `"unknown"` fallback merged every
/// such document into one doc id.
pub fn doc_id_from_hint(path: &str) -> Result<String, ApiError> {
    // A storage key or file name from the client: '/'-separated on every
    // platform. Parsed as text, not as an OS path (on Windows `Path` also
    // splits on '\', which would file `a\b` under `b`).
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.is_empty() || name == "." || name == ".." {
        return Err(ApiError::bad_request(format!(
            "`path` {path:?} has no document name"
        )));
    }
    // `Path::file_stem` semantics: drop the last extension, but a leading
    // dot (".hidden") is part of the name.
    let stem = match name.rfind('.') {
        Some(0) | None => name,
        Some(i) => &name[..i],
    };
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
    use crate::testutil::{fake_state, prose, FakeOllama, FakeStore, OllamaMode, Op};
    use hs_common::service::protocol::ReadinessInfo;

    struct Harness {
        state: DistillServerState,
        store: Arc<FakeStore>,
        ollama: FakeOllama,
    }

    async fn harness() -> Harness {
        let (state, store, ollama) = fake_state().await;
        Harness {
            state,
            store,
            ollama,
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
        let h = harness().await;
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

        assert_eq!(h.store.ops(), [], "nothing may be stored");
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn index_requests_need_both_a_name_and_content() {
        let h = harness().await;
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
        let h = harness().await;
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
        assert_eq!(result.embedding_device, "cuda");
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

    #[tokio::test]
    async fn only_configured_collections_resolve() {
        let h = harness().await;
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
        let h = harness().await;
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

        assert_eq!(h.store.ops(), [], "nothing may be written or deleted");
        assert!(h.store.state.lock().searches.is_empty());
        assert!(h.ollama.http.recorded().is_empty());
    }

    #[tokio::test]
    async fn reset_deletes_only_the_documents_of_the_named_configured_collection() {
        let h = harness().await;
        h.store.seed("academic_papers", "a", 3);
        h.store.seed("paper_abstracts", "a", 1);
        h.store.seed("paper_abstracts", "b", 2);
        let (name, deleted) = h
            .state
            .reset_collection(Some("paper_abstracts"))
            .await
            .unwrap();
        assert_eq!((name.as_str(), deleted), ("paper_abstracts", 3));
        assert_eq!(h.store.stored("academic_papers", "a").len(), 3);
        assert!(h.store.stored("paper_abstracts", "a").is_empty());
        assert_eq!(
            h.store.ops(),
            [
                Op::Forget {
                    collection: "paper_abstracts".into(),
                    doc_id: "a".into(),
                    keep: vec![]
                },
                Op::Forget {
                    collection: "paper_abstracts".into(),
                    doc_id: "b".into(),
                    keep: vec![]
                },
            ]
        );
    }

    // ── RA-89: search and listing limits ───────────────────────────────

    #[tokio::test]
    async fn an_unparseable_year_filter_is_a_400_not_an_unfiltered_search() {
        let h = harness().await;
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
    }

    #[tokio::test]
    async fn search_limit_defaults_and_is_clamped() {
        // The store is asked for exactly the page the caller wants; it
        // does the candidate pooling and reranking itself.
        let h = harness().await;
        h.store.state.lock().canned_hits =
            (0..300).map(|i| hit(&format!("d{i}"), "text")).collect();

        let mut got = Vec::new();
        for limit in [None, Some(25), Some(u64::MAX)] {
            got.push(
                h.state
                    .search(search_req("q", limit, None))
                    .await
                    .unwrap()
                    .len(),
            );
        }
        assert_eq!(
            got,
            [DEFAULT_SEARCH_LIMIT as usize, 25, MAX_SEARCH_LIMIT as usize]
        );
        let limits: Vec<u64> = h.store.state.lock().searches.iter().map(|s| s.2).collect();
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
    async fn an_oversized_search_query_is_rejected_before_it_reaches_the_store() {
        let h = harness().await;
        let at_cap = "q".repeat(MAX_SEARCH_QUERY_BYTES);
        h.state
            .search(search_req(&at_cap, None, None))
            .await
            .unwrap();
        assert_eq!(h.store.state.lock().searches.len(), 1);

        let over = "q".repeat(MAX_SEARCH_QUERY_BYTES + 1);
        assert_eq!(
            kind(h.state.search(search_req(&over, None, None)).await),
            ErrorKind::BadRequest
        );
        assert_eq!(
            h.store.state.lock().searches.len(),
            1,
            "the oversized query reached the store"
        );
    }

    #[tokio::test]
    async fn search_forwards_the_query_the_collection_and_the_parsed_filters() {
        let h = harness().await;
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
        let (collection, query, limit, filter) = &st.searches[0];
        assert_eq!(
            (collection.as_str(), query.as_str(), *limit),
            ("academic_papers", "find me", 5)
        );
        assert_eq!(filter.year, Some(crate::store::YearFilter::Gte(2020)));
        assert_eq!(filter.topic.as_deref(), Some("autism"));
    }

    fn hit(doc_id: &str, text: &str) -> SearchHit {
        SearchHit {
            doc_id: doc_id.into(),
            title: None,
            authors: vec![],
            year: None,
            doi: None,
            chunk_text: text.into(),
            score: 0.0,
            pdf_path: None,
            line_start: 0,
            line_end: 0,
            page: None,
            category: None,
        }
    }

    async fn search_ids(h: &Harness, hits: Vec<SearchHit>, limit: u64) -> Vec<String> {
        h.store.state.lock().canned_hits = hits;
        h.state
            .search(search_req("q", Some(limit), None))
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.doc_id)
            .collect()
    }

    // ── reference-list demotion ────────────────────────────────────

    /// A chunk that is a list of citations.
    fn bibliography_chunk(doc_id: &str) -> SearchHit {
        hit(
            doc_id,
            "Berkenbosch F, van Vant J, Aubert A, et al. (2001) Neurotrophins and the brain. \
             Nature Reviews Neuroscience.\n\
             van Vant J, Jonkman T, Caligor L (2004) Neurobiology of learning and memory. \
             Journal of Neurochemistry, vol. 104, pp. 12-19.",
        )
    }

    #[tokio::test]
    async fn a_reference_list_is_demoted_below_prose_that_outranks_it() {
        // A bibliography entry matches a topic query because a citation to
        // work about the topic *is* about the topic. When prose outranks
        // it, the passage that discusses the topic must come first.
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![
                hit(
                    "prose",
                    "The hippocampus generates new neurons throughout adulthood.",
                ),
                bibliography_chunk("references"),
            ],
            10,
        )
        .await;
        assert_eq!(ids, ["prose", "references"]);
    }

    #[tokio::test]
    async fn a_demoted_reference_list_is_still_returned_with_its_score() {
        // A user asking for a paper's bibliography wants it: the chunk is
        // demoted, not dropped.
        let h = harness().await;
        let mut references = bibliography_chunk("references");
        references.score = 0.42;
        h.store.state.lock().canned_hits = vec![
            references,
            hit(
                "prose",
                "The hippocampus generates new neurons throughout adulthood.",
            ),
        ];
        let hits = h
            .state
            .search(search_req("hippocampus neurogenesis", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "the reference list was dropped, not demoted");
        assert_eq!(hits[1].doc_id, "references");
        assert!(
            (hits[1].score - 0.42).abs() < 1e-6,
            "the demoted hit lost its score"
        );
    }

    #[tokio::test]
    async fn a_mixed_chunk_is_not_demoted() {
        // A chunk that cites a work and then discusses it is useful, and
        // must not be treated as a reference list.
        let h = harness().await;
        let ids =
            search_ids(
                &h,
                vec![
                hit(
                    "mixed",
                    "Neurogenesis was first reported by Altman (1962) in rodents, and the rate of \
                     new neuron production in the adult hippocampus remains contested to this day.",
                ),
                hit("other", "Something else entirely about a different subject."),
            ],
                10,
            )
            .await;
        assert_eq!(ids[0], "mixed", "a mixed chunk was demoted");
    }

    #[tokio::test]
    async fn the_demotion_does_not_change_the_number_of_results() {
        let h = harness().await;
        let ids = search_ids(
            &h,
            (0..4)
                .map(|i| bibliography_chunk(&format!("refs{i}")))
                .collect(),
            10,
        )
        .await;
        assert_eq!(ids.len(), 4, "results were dropped");
    }

    #[tokio::test]
    async fn a_small_limit_still_demotes_a_reference_list() {
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![
                bibliography_chunk("references"),
                hit(
                    "prose-1",
                    "The split brain was studied by Sperry and Gazzaniga.",
                ),
                hit(
                    "prose-2",
                    "A second prose passage about hemispheric specialization.",
                ),
            ],
            3,
        )
        .await;
        assert_eq!(ids[0], "prose-1", "the prose did not come first: {ids:?}");
        assert!(
            ids.contains(&"references".to_string()),
            "the reference list was dropped, not demoted: {ids:?}"
        );
    }

    #[tokio::test]
    async fn a_reference_list_leads_only_when_there_is_no_prose() {
        // Sometimes the only chunk in the corpus that mentions the term
        // IS a reference list; with nothing else to show, it is the answer
        // and keeps its place.
        let h = harness().await;
        let ids = search_ids(&h, vec![bibliography_chunk("references")], 3).await;
        assert_eq!(ids[0], "references");
    }

    #[tokio::test]
    async fn prose_leads_even_when_a_reference_list_outranks_it() {
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![
                bibliography_chunk("references"),
                hit("prose", "Something about the subject matter."),
            ],
            3,
        )
        .await;
        assert_eq!(ids[0], "prose");
    }

    #[tokio::test]
    async fn a_bibliography_in_a_bare_year_style_is_demoted_below_prose() {
        // The year is not in parentheses. Measured 2026-10-21, this shape
        // was the first hit for 6 of the 25 fixed queries.
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![
                hit(
                    "bibliography",
                    "Sundralingham, P. 1983 Responses of potted seedlings of Dryobalanops \
                     aromatica to shading. Malaysian Forester.\n\
                     Tanner, E. V. J., Vitousek, P. M. & Cuevas, E. 1998 Experimental \
                     investigation of nutrient limitation. Ecology 79:744-52.\n\
                     Trenberth, K. E. 1997 The definition of El Nino. Bull. Am. Meteorol. Soc.",
                ),
                hit("prose", "Something about the subject matter."),
            ],
            3,
        )
        .await;
        assert_eq!(ids[0], "prose");
    }

    #[tokio::test]
    async fn a_bibliography_in_a_numbered_style_is_demoted_below_prose() {
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![
                hit(
                    "bibliography",
                    "59. Ferguson JN, Aldag JM, Insel TR, Young LJ. Oxytocin in the medial \
                     amygdala is essential for social recognition. J Neurosci 2009.\n\
                     71. Melis MR, Succu S, Mascia MS, Cortis L, Argiolas A. Extra-cellular \
                     dopamine increases in the paraventricular nucleus. Neurosci Lett 2003.\n\
                     72. Waldherr M, Neumann ID. Centrally released oxytocin mediates mating-induced \
                     anxiolysis in male rats. Proc Natl Acad Sci 2007.",
                ),
                hit("prose", "Something about the subject matter."),
            ],
            3,
        )
        .await;
        assert_eq!(ids[0], "prose");
    }

    #[tokio::test]
    async fn a_numbered_prose_paragraph_is_not_demoted() {
        // "22. The idea of the series is, in itself, important …" is a
        // numbered footnote, not a bibliography.
        let h = harness().await;
        let ids = search_ids(
            &h,
            vec![hit(
                "prose",
                "22. The idea of the series is, in itself, important to Eisenman's early \
                 development. Sandra Kaji-O'Gorman explores the relationship in detail.\n\
                 19. Eisenman is in an interesting tension with Bergson here, as he often was \
                 in his writings about the nature of form.",
            )],
            3,
        )
        .await;
        assert_eq!(ids[0], "prose");
    }

    #[tokio::test]
    async fn list_docs_limit_is_bounded_and_never_silently_truncated() {
        let h = harness().await;
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
    async fn status_counts_chunks_and_documents() {
        let h = harness().await;
        h.store.seed("academic_papers", "a", 3);
        h.store.seed("academic_papers", "b", 2);
        h.store.seed("paper_abstracts", "c", 7);
        let s = h.state.status(None).await.unwrap();
        assert_eq!((s.points_count, s.documents_count), (5, 2));
        assert!(!s.documents_count_truncated);
        assert_eq!(s.embed_model, "bge-m3:latest");
        assert_eq!(s.compute_device, "cuda");
        assert_eq!(s.collection, "academic_papers");
    }

    #[tokio::test]
    async fn status_reports_a_cpu_embedder_rather_than_hiding_it() {
        let h = harness().await;
        *h.ollama.mode.lock() = OllamaMode::Cpu;
        assert_eq!(h.state.status(None).await.unwrap().compute_device, "cpu");
    }

    #[tokio::test]
    async fn readiness_reports_capacity_and_real_health() {
        let h = harness().await;
        let r = h.state.readiness().await;
        assert!(r.ready && r.reason.is_none());
        assert_eq!(r.capacity, 1, "writes are serialized");
        assert_eq!(r.available_slots(), 1);

        h.state.in_flight.store(5, Ordering::Relaxed);
        let r = h.state.readiness().await;
        assert_eq!(
            (r.in_flight, r.available_slots()),
            (5, 0),
            "over capacity is no slots, not underflow"
        );
        h.state.in_flight.store(0, Ordering::Relaxed);

        for (mode, want) in [
            (OllamaMode::Cpu, "embedder not on GPU"),
            (OllamaMode::Partial, "embedder not on GPU"),
            (OllamaMode::Down, "embedder unavailable"),
        ] {
            *h.ollama.mode.lock() = mode;
            let r = h.state.readiness().await;
            assert!(!r.ready, "{mode:?}");
            assert_eq!(r.available_slots(), 0);
            let reason = r.reason.unwrap();
            assert_eq!(reason, want, "{mode:?}");
            assert!(!reason.contains("ollama is down"), "cause leaked: {reason}");
        }

        *h.ollama.mode.lock() = OllamaMode::Cuda;
        h.store.state.lock().down = true;
        let r = h.state.readiness().await;
        assert!(!r.ready);
        assert!(r.reason.unwrap().contains("myelin"));
    }

    #[tokio::test]
    async fn health_fails_with_503_semantics_when_a_dependency_is_down() {
        let h = harness().await;
        let ok = h.state.health().await.unwrap();
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.compute_device, "cuda");
        assert_eq!(ok.embed_model, "bge-m3:latest");
        assert_eq!(ok.backend_version, "fake-myelin 1.0.0");

        for mode in [OllamaMode::Cpu, OllamaMode::Partial, OllamaMode::Down] {
            *h.ollama.mode.lock() = mode;
            assert_eq!(kind(h.state.health().await), ErrorKind::Unavailable);
        }

        *h.ollama.mode.lock() = OllamaMode::Cuda;
        h.store.state.lock().down = true;
        assert_eq!(kind(h.state.health().await), ErrorKind::Unavailable);
    }

    /// F10/F11: the open probe must not reveal the myelin address or the
    /// cause of a failure; the protected `/status` carries the address.
    #[tokio::test]
    async fn the_open_health_probe_leaks_neither_address_nor_cause() {
        let h = harness().await;
        let ok = serde_json::to_string(&h.state.health().await.unwrap()).unwrap();
        assert!(
            !ok.contains("myelin_url") && !ok.contains(&h.state.config.myelin_url),
            "{ok}"
        );

        *h.ollama.mode.lock() = OllamaMode::Down;
        let err = h.state.health().await.unwrap_err().to_string();
        assert!(!err.contains("ollama is down"), "{err}");
        assert!(!err.contains(&h.ollama.http.url()), "{err}");

        *h.ollama.mode.lock() = OllamaMode::Cuda;
        h.store.state.lock().down = true;
        let err = h.state.health().await.unwrap_err().to_string();
        assert!(err.contains("myelin unavailable"), "{err}");
        assert!(!err.contains("refused"), "{err}");
        h.store.state.lock().down = false;

        let status = h.state.status(None).await.unwrap();
        assert_eq!(status.myelin_url, h.state.config.myelin_url);
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
    fn bad_input_is_a_400_a_backend_failure_a_503_and_everything_else_a_500() {
        assert_eq!(
            ApiError::from(DistillError::InvalidInput("x".into())).kind,
            ErrorKind::BadRequest
        );
        assert_eq!(
            ApiError::from(DistillError::Backend("x".into())).kind,
            ErrorKind::Unavailable
        );
        for e in [
            DistillError::Metadata("x".into()),
            DistillError::Config("x".into()),
        ] {
            assert_eq!(ApiError::from(e).kind, ErrorKind::Internal);
        }
    }

    #[tokio::test]
    async fn a_store_failure_while_indexing_is_a_503_not_a_400() {
        let h = harness().await;
        h.store.state.lock().fail_write_from_call = Some(0);
        let job = h
            .state
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), None))
            .unwrap();
        assert_eq!(
            kind(h.state.run_index(job, |_| {}).await),
            ErrorKind::Unavailable
        );
    }

    #[tokio::test]
    async fn indexing_is_refused_while_the_embedding_model_is_off_the_gpu() {
        let h = harness().await;
        for mode in [OllamaMode::Cpu, OllamaMode::Partial, OllamaMode::Down] {
            *h.ollama.mode.lock() = mode;
            let job = h
                .state
                .prepare_index(index_req(Some("d.md"), Some(&prose(6)), None))
                .unwrap();
            assert_eq!(
                kind(h.state.run_index(job, |_| {}).await),
                ErrorKind::Unavailable,
                "{mode:?}"
            );
        }
        assert_eq!(h.store.ops(), [], "nothing may be written");
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
    }

    // ── per-document operations ────────────────────────────────────────

    #[tokio::test]
    async fn delete_reports_how_many_chunks_existed_and_removes_them() {
        let h = harness().await;
        h.store.seed("academic_papers", "d", 4);
        assert_eq!(h.state.delete_doc("d", None).await.unwrap(), 4);
        assert!(h.store.stored("academic_papers", "d").is_empty());
        assert_eq!(h.state.delete_doc("d", None).await.unwrap(), 0);
        assert_eq!(h.state.doc_exists("d", None).await.unwrap(), (false, 0));
    }

    #[tokio::test]
    async fn doc_exists_counts_the_documents_own_chunks() {
        let h = harness().await;
        h.store.seed("academic_papers", "d", 4);
        h.store.seed("academic_papers", "other", 9);
        assert_eq!(h.state.doc_exists("d", None).await.unwrap(), (true, 4));
    }

    #[tokio::test]
    async fn doc_ids_that_could_traverse_are_rejected() {
        let h = harness().await;
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
        let h = harness().await;
        h.store.state.lock().panic_on_write = true;
        let job = h
            .state
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), None))
            .unwrap();
        let err = h.state.run_index(job, |_| {}).await.expect_err("must fail");
        assert_eq!(err.kind, ErrorKind::Panicked);
        assert!(
            err.message.starts_with(PANIC_CODE) && err.message.contains("d"),
            "{err}"
        );
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(h.store.ops(), [], "nothing written");
    }
}
