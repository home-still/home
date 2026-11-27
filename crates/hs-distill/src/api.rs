//! Transport-independent request handling for the distill server.
//!
//! `server` is a thin axum adapter over [`DistillServerState`]; everything
//! that decides what a request means — validation, store routing,
//! limits, error classification — lives here so it runs (and is tested)
//! without a socket, a GPU, or a myelin.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hs_common::service::inflight::InFlightGuard;
use hs_common::stores::{DerivedText, StoreKind, StoreSpec, Stores};
use serde::Deserialize;

use crate::client::{
    HealthResponse, IndexResult, ReadinessResponse, SearchFilters, SearchHit, StatusResponse,
    MAX_DOC_LIST_LIMIT, MAX_SEARCH_LIMIT,
};
use crate::config::DistillServerConfig;
use crate::embed_device::{ollama_embed_device, DEVICE_CUDA, OLLAMA_EMBED_MODEL};
use crate::error::DistillError;
use crate::pipeline::{self, ContentProfile, IndexJob};
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
/// task (`x-hs-error-code` header; prefix of the error message).
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

/// Body of `POST /distill`.
#[derive(Deserialize)]
pub struct IndexRequest {
    /// Document name. Only its file stem is used (it becomes the
    /// `doc_id`); the file it names is never opened.
    pub path: Option<String>,
    /// The markdown to index. Required: the server indexes the text it is
    /// sent and never reads documents from disk.
    pub content: Option<String>,
    /// Catalog entry for the document. Without one the chunks carry no
    /// title/authors/DOI/year.
    pub catalog: Option<hs_common::catalog::CatalogEntry>,
    /// The store to index into, by name. Required: it must be a store
    /// documents are sent to, not one filled from another store's
    /// documents.
    #[serde(default)]
    pub store: Option<String>,
}

/// Body of `POST /search`.
#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    /// Hits wanted (default 10, at most [`MAX_SEARCH_LIMIT`]: larger values
    /// are clamped).
    pub limit: Option<u64>,
    pub filters: Option<SearchFilters>,
    /// The stores to search, by name. None named means every store a read
    /// covers by default.
    #[serde(default)]
    pub stores: Vec<String>,
}

/// A validated index request.
pub struct PreparedIndex {
    doc_id: String,
    content: String,
    catalog: Option<hs_common::catalog::CatalogEntry>,
    store: &'static str,
    /// The store the document's reference-list chunks go to.
    references: Option<&'static str>,
    profile: ContentProfile,
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
    /// The registry of stores. A request names one of them; the server
    /// serves every one that holds documents.
    pub stores: Stores,
    pub in_flight: Arc<AtomicUsize>,
}

impl DistillServerState {
    pub fn new(
        store: Arc<dyn DocumentStore>,
        ollama: reqwest::Client,
        config: DistillServerConfig,
        stores: Stores,
    ) -> Self {
        Self {
            store,
            ollama,
            config,
            stores,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Bind every store of the registry to its tenant in the backend, the
    /// memory store included: the registry says which tenant each name
    /// means, for an agent reading myelin as much as for this server. The
    /// server calls this before it listens and refuses to start on an
    /// `Err`: a name the backend holds for another tenant would file
    /// documents where no reader looks.
    pub async fn define_stores(&self) -> Result<(), ApiError> {
        for spec in self.stores.iter() {
            self.store.define_store(spec).await.map_err(|e| {
                ApiError::unavailable(format!(
                    "cannot define store {:?} (tenant {:?}): {e}",
                    spec.name, spec.tenant
                ))
            })?;
        }
        Ok(())
    }

    /// The stores this server serves: every store of the registry that
    /// holds documents. The memory store holds statements an agent wrote,
    /// which reach myelin without this server.
    fn served_stores(&self) -> impl Iterator<Item = &StoreSpec> {
        self.stores
            .iter()
            .filter(|spec| !matches!(spec.kind, StoreKind::Memory))
    }

    fn served_names(&self) -> String {
        self.served_stores()
            .map(|spec| spec.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The store a request names. It must be named, and be one this server
    /// serves: a request can never reach an arbitrary myelin tenant by
    /// naming it.
    pub fn resolve_store(&self, requested: Option<&str>) -> Result<&StoreSpec, ApiError> {
        let name = requested.ok_or_else(|| {
            ApiError::bad_request(format!(
                "`store` is required; this server serves: {}",
                self.served_names()
            ))
        })?;
        self.served_stores()
            .find(|spec| spec.name == name)
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "unknown store {name:?}; this server serves: {}",
                    self.served_names()
                ))
            })
    }

    /// The store that holds the reference-list chunks of the documents of
    /// `spec`, when it has one.
    fn reference_store(&self, spec: &StoreSpec) -> Option<&'static str> {
        self.stores
            .derived(spec.name, DerivedText::ReferenceChunks)
            .map(|child| child.name)
    }

    /// Every store that holds chunks of the documents of `spec`: the store
    /// itself and its reference store.
    fn chunk_stores(&self, spec: &StoreSpec) -> impl Iterator<Item = &'static str> {
        std::iter::once(spec.name).chain(self.reference_store(spec))
    }

    /// A store documents are sent to and deleted from, and the gates for
    /// what it is sent. A store of reference lists is refused: its records
    /// are written and deleted with the documents they come from, so that
    /// a document's chunks are always counted whole.
    fn document_store(
        &self,
        requested: Option<&str>,
    ) -> Result<(&StoreSpec, ContentProfile), ApiError> {
        let spec = self.resolve_store(requested)?;
        let profile = ContentProfile::for_kind(&spec.kind).ok_or_else(|| {
            ApiError::bad_request(format!(
                "the store {:?} takes no documents of its own: it is filled and emptied with \
                 the documents of the store it is derived from",
                spec.name
            ))
        })?;
        Ok((spec, profile))
    }

    /// doc_id -> its chunks, counted over every store that holds chunks of
    /// the documents of `spec`.
    async fn chunk_counts(
        &self,
        spec: &StoreSpec,
        doc_id: Option<&str>,
    ) -> Result<BTreeMap<String, u64>, ApiError> {
        let mut counts = BTreeMap::new();
        for store in self.chunk_stores(spec) {
            for (doc, chunks) in self.store.inventory(store, doc_id).await? {
                *counts.entry(doc).or_insert(0) += chunks;
            }
        }
        Ok(counts)
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
        let (spec, profile) = self.document_store(req.store.as_deref())?;
        Ok(PreparedIndex {
            doc_id,
            content,
            catalog: req.catalog,
            store: spec.name,
            references: self.reference_store(spec),
            profile,
        })
    }

    /// Chunk and store a prepared document.
    pub async fn run_index(&self, job: PreparedIndex) -> Result<IndexResult, ApiError> {
        let _guard = InFlightGuard::new(&self.in_flight);
        let PreparedIndex {
            doc_id,
            content,
            catalog,
            store,
            references,
            profile,
        } = job;
        // Embedding happens inside the store's write; refuse to start one
        // while the model is not on the GPU.
        let device = self.require_gpu_embedder().await?;
        let work = pipeline::index_document(
            IndexJob {
                doc_id: &doc_id,
                content: &content,
                catalog,
                store,
                references,
                profile,
            },
            &self.config,
            self.store.as_ref(),
        );
        // A panic here would otherwise become an untyped 500: surface it as a
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

    /// The stores a search reads: the ones it names, each once, or with
    /// none named every served store a read covers by default.
    fn search_stores(&self, requested: &[String]) -> Result<Vec<&'static str>, ApiError> {
        if requested.is_empty() {
            return Ok(self
                .served_stores()
                .filter(|spec| spec.default_read)
                .map(|spec| spec.name)
                .collect());
        }
        let mut names = Vec::with_capacity(requested.len());
        for name in requested {
            let spec = self.resolve_store(Some(name))?;
            if !names.contains(&spec.name) {
                names.push(spec.name);
            }
        }
        Ok(names)
    }

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
        let stores = self.search_stores(&req.stores)?;
        let filter = SearchFilter::from_request(req.filters.as_ref())?;
        let limit = match req.limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(0) => return Err(ApiError::bad_request("limit must be at least 1")),
            Some(n) => n.min(MAX_SEARCH_LIMIT),
        };

        // The store ranks: dense and BM25 candidates fused by rank, then
        // reordered by a cross-encoder, over every named store at once.
        Ok(self
            .store
            .search(&stores, &req.query, limit, &filter)
            .await?)
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

    /// What a store holds. A document's chunks are counted whole: the ones
    /// in the store and the reference lists kept beside it.
    pub async fn status(&self, store: Option<&str>) -> Result<StatusResponse, ApiError> {
        let spec = self.resolve_store(store)?;
        let counts = self.chunk_counts(spec, None).await?;
        Ok(StatusResponse {
            store: spec.name.to_string(),
            records_count: counts.values().sum(),
            documents_count: counts.len() as u64,
            documents_count_truncated: false,
            compute_device: self.embedder_device().await?,
            embed_model: OLLAMA_EMBED_MODEL.into(),
            myelin_url: self.config.myelin_url.clone(),
        })
    }

    // ── per-document and maintenance operations ────────────────────

    /// Whether `doc_id` has chunks, and how many (reference lists
    /// included).
    pub async fn doc_exists(
        &self,
        doc_id: &str,
        store: Option<&str>,
    ) -> Result<(bool, u64), ApiError> {
        validate_doc_id(doc_id)?;
        let spec = self.resolve_store(store)?;
        let counts = self.chunk_counts(spec, Some(doc_id)).await?;
        let chunks = counts.get(doc_id).copied().unwrap_or(0);
        Ok((chunks > 0, chunks))
    }

    /// Delete every chunk of `doc_id`, its reference lists included;
    /// returns how many existed.
    pub async fn delete_doc(&self, doc_id: &str, store: Option<&str>) -> Result<u64, ApiError> {
        validate_doc_id(doc_id)?;
        let (spec, _) = self.document_store(store)?;
        let deleted =
            pipeline::remove_document(self.store.as_ref(), self.chunk_stores(spec), doc_id).await?;
        Ok(deleted)
    }

    /// Distinct doc ids. `limit` defaults to, and may not exceed,
    /// [`MAX_DOC_LIST_LIMIT`]; a request above it is rejected rather than
    /// silently truncated, because callers diff this list against storage.
    pub async fn list_docs(
        &self,
        limit: Option<u64>,
        store: Option<&str>,
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
        let spec = self.resolve_store(store)?;
        let mut doc_ids: Vec<String> = self.chunk_counts(spec, None).await?.into_keys().collect();
        let truncated = doc_ids.len() as u64 > limit;
        doc_ids.truncate(limit as usize);
        Ok(DocList { doc_ids, truncated })
    }

    /// Delete every document of a store, reference lists included.
    /// Destructive; the route is behind the backend-token middleware (see
    /// `server.rs`). Returns the store and how many chunks were deleted.
    pub async fn reset_store(&self, store: Option<&str>) -> Result<(&'static str, u64), ApiError> {
        let (spec, _) = self.document_store(store)?;
        let mut deleted = 0;
        for holder in self.chunk_stores(spec) {
            let inventory = self.store.inventory(holder, None).await?;
            for doc_id in inventory.keys() {
                deleted += self.store.forget_document(holder, doc_id, &[]).await?;
            }
        }
        Ok((spec.name, deleted))
    }
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
    use crate::testutil::{fake_state, prose, FakeOllama, FakeStore, OllamaMode, Op, BIBLIOGRAPHY};
    use hs_common::markdown::PAGE_SEPARATOR;
    use hs_common::service::protocol::ReadinessInfo;
    use hs_common::stores::{ABSTRACTS, ACADEMIC, MEMORY, PERSONAL, REFERENCES, SYNTHESES, WEB};

    /// The store most tests address.
    const IN_ACADEMIC: Option<&str> = Some(ACADEMIC);

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

    fn index_req(path: Option<&str>, content: Option<&str>, store: Option<&str>) -> IndexRequest {
        IndexRequest {
            path: path.map(str::to_string),
            content: content.map(str::to_string),
            catalog: None,
            store: store.map(str::to_string),
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
            stores: vec![],
        }
    }

    fn search_in(stores: &[&str]) -> SearchRequest {
        SearchRequest {
            stores: stores.iter().map(|s| s.to_string()).collect(),
            ..search_req("q", None, None)
        }
    }

    fn kind<T>(r: Result<T, ApiError>) -> ErrorKind {
        match r {
            Err(e) => e.kind,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn refusal<T>(r: Result<T, ApiError>) -> ApiError {
        match r {
            Err(e) => e,
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

        let err = refusal(
            h.state
                .prepare_index(index_req(secret.to_str(), None, IN_ACADEMIC)),
        );
        assert_eq!(err.kind, ErrorKind::BadRequest);
        assert!(err.message.contains("content"), "{err}");

        assert_eq!(h.store.ops(), [], "nothing may be stored");
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn index_requests_need_a_name_content_and_a_store() {
        let h = harness().await;
        for (path, content, store, missing) in [
            (None, Some("x"), IN_ACADEMIC, "`path`"),
            (Some("a.md"), None, IN_ACADEMIC, "`content`"),
            (None, None, IN_ACADEMIC, "`content`"),
            (Some("a.md"), Some("x"), None, "`store`"),
        ] {
            let err = refusal(h.state.prepare_index(index_req(path, content, store)));
            assert_eq!(err.kind, ErrorKind::BadRequest, "{err}");
            assert!(err.message.contains(missing), "{err}");
        }
    }

    #[tokio::test]
    async fn content_is_indexed_under_the_stem_of_the_name() {
        let h = harness().await;
        let job = h
            .state
            .prepare_index(index_req(
                Some("markdown/ab/10.1609_aaai.v38i16.29728.md"),
                Some(&prose(6)),
                IN_ACADEMIC,
            ))
            .unwrap();
        let result = h.state.run_index(job).await.unwrap();
        assert_eq!(result.doc_id, "10.1609_aaai.v38i16.29728");
        assert!(result.chunks_indexed > 0);
        assert_eq!(result.embedding_device, "cuda");
        assert_eq!(
            h.store.stored(ACADEMIC, "10.1609_aaai.v38i16.29728").len() as u32,
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

    // ── RA-26: only the registry's document stores are served ──────────

    #[tokio::test]
    async fn only_the_stores_that_hold_documents_resolve() {
        let h = harness().await;
        for name in [ACADEMIC, REFERENCES, ABSTRACTS, PERSONAL, WEB, SYNTHESES] {
            assert_eq!(h.state.resolve_store(Some(name)).unwrap().name, name);
        }
        for name in [
            "other",
            "",
            "academic ",
            "ACADEMIC",
            // A myelin tenant is not a store name.
            "academic_papers",
            "../x",
            "x'; drop",
            // Statements an agent wrote, not documents.
            MEMORY,
        ] {
            let e = h.state.resolve_store(Some(name)).expect_err(name);
            assert_eq!(e.kind, ErrorKind::BadRequest, "{name:?}");
            assert!(e.message.contains("this server serves:"), "{e}");
            assert!(e.message.contains(PERSONAL), "{e}");
        }
        let e = h.state.resolve_store(None).expect_err("no store named");
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("`store` is required"), "{e}");
    }

    #[tokio::test]
    async fn every_store_of_the_registry_is_defined_and_a_refusal_is_an_error() {
        let h = harness().await;
        h.state.define_stores().await.unwrap();
        let mut defined: Vec<(&str, &str)> = h
            .store
            .state
            .lock()
            .defined
            .iter()
            .map(|spec| (spec.name, spec.tenant))
            .collect();
        defined.sort_unstable();
        assert_eq!(
            defined,
            [
                (ABSTRACTS, "paper_abstracts"),
                (ACADEMIC, "academic_papers"),
                (MEMORY, "user_memory"),
                (PERSONAL, "personal_docs"),
                (REFERENCES, "academic_references"),
                (SYNTHESES, "syntheses"),
                (WEB, "web_pages"),
            ]
        );

        h.store.state.lock().fail_define = true;
        let e = h.state.define_stores().await.expect_err("refused");
        assert_eq!(e.kind, ErrorKind::Unavailable);
        assert!(
            e.message.contains("cannot define store") && e.message.contains("define refused"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn every_operation_rejects_an_unserved_store_without_touching_the_store() {
        let h = harness().await;

        for c in [Some("not_a_store"), Some(MEMORY), None] {
            assert_eq!(
                kind(h.state.prepare_index(index_req(Some("a.md"), Some("x"), c))),
                ErrorKind::BadRequest,
                "{c:?}"
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
            assert_eq!(kind(h.state.reset_store(c).await), ErrorKind::BadRequest);
        }
        for name in ["not_a_store", MEMORY] {
            // One bad name refuses the whole search, not just its share.
            assert_eq!(
                kind(h.state.search(search_in(&[ACADEMIC, name])).await),
                ErrorKind::BadRequest,
                "{name}"
            );
        }

        assert_eq!(h.store.ops(), [], "nothing may be written or deleted");
        assert!(h.store.state.lock().searches.is_empty());
        assert!(h.ollama.http.recorded().is_empty());
    }

    #[tokio::test]
    async fn a_reference_store_is_read_but_takes_and_loses_no_documents_of_its_own() {
        let h = harness().await;
        h.store.seed(REFERENCES, "d", 2);
        let c = Some(REFERENCES);

        for err in [
            refusal(h.state.prepare_index(index_req(Some("d.md"), Some("x"), c))),
            refusal(h.state.delete_doc("d", c).await),
            refusal(h.state.reset_store(c).await),
        ] {
            assert_eq!(err.kind, ErrorKind::BadRequest, "{err}");
            assert!(err.message.contains("no documents of its own"), "{err}");
        }
        assert_eq!(h.store.ops(), [], "nothing may be written or deleted");

        assert_eq!(h.state.doc_exists("d", c).await.unwrap(), (true, 2));
        assert_eq!(h.state.list_docs(None, c).await.unwrap().doc_ids, ["d"]);
        let status = h.state.status(c).await.unwrap();
        assert_eq!((status.records_count, status.documents_count), (2, 1));
    }

    #[tokio::test]
    async fn reset_deletes_the_documents_of_the_named_store_and_their_reference_lists() {
        let h = harness().await;
        h.store.seed(ACADEMIC, "a", 3);
        h.store.seed(REFERENCES, "a", 2);
        h.store.seed(ABSTRACTS, "a", 1);
        h.store.seed(PERSONAL, "p", 4);

        let (name, deleted) = h.state.reset_store(IN_ACADEMIC).await.unwrap();
        assert_eq!((name, deleted), (ACADEMIC, 5));
        assert!(h.store.stored(ACADEMIC, "a").is_empty());
        assert!(h.store.stored(REFERENCES, "a").is_empty());
        assert_eq!(h.store.stored(ABSTRACTS, "a").len(), 1);
        assert_eq!(h.store.stored(PERSONAL, "p").len(), 4);
        assert_eq!(
            h.store.ops(),
            [
                Op::Forget {
                    store: ACADEMIC.into(),
                    doc_id: "a".into(),
                    keep: vec![]
                },
                Op::Forget {
                    store: REFERENCES.into(),
                    doc_id: "a".into(),
                    keep: vec![]
                },
            ]
        );

        let (name, deleted) = h.state.reset_store(Some(ABSTRACTS)).await.unwrap();
        assert_eq!((name, deleted), (ABSTRACTS, 1));
        assert_eq!(h.store.stored(PERSONAL, "p").len(), 4);
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
    async fn search_forwards_the_query_the_stores_and_the_parsed_filters() {
        let h = harness().await;
        let filters = SearchFilters {
            year: Some(">=2020".into()),
            topic: Some("autism".into()),
            category: None,
        };
        h.state
            .search(SearchRequest {
                stores: vec![PERSONAL.into()],
                ..search_req("find me", Some(5), Some(filters))
            })
            .await
            .unwrap();
        let st = h.store.state.lock();
        let (stores, query, limit, filter) = &st.searches[0];
        assert_eq!(*stores, [PERSONAL]);
        assert_eq!((query.as_str(), *limit), ("find me", 5));
        assert_eq!(filter.year, Some(crate::store::YearFilter::Gte(2020)));
        assert_eq!(filter.topic.as_deref(), Some("autism"));
    }

    #[tokio::test]
    async fn a_search_that_names_no_store_reads_the_default_ones() {
        // The papers are read by default. Their reference lists are kept
        // in a store of their own so that a default search does NOT read
        // them.
        let h = harness().await;
        h.state.search(search_req("q", None, None)).await.unwrap();
        let st = h.store.state.lock();
        let stores = &st.searches[0].0;
        assert!(stores.iter().any(|s| s == ACADEMIC), "{stores:?}");
        for never in [REFERENCES, MEMORY] {
            assert!(!stores.iter().any(|s| s == never), "{stores:?}");
        }
    }

    #[tokio::test]
    async fn a_search_reads_each_named_store_once_in_the_order_named() {
        let h = harness().await;
        h.state
            .search(search_in(&[REFERENCES, ACADEMIC, REFERENCES]))
            .await
            .unwrap();
        assert_eq!(h.store.state.lock().searches[0].0, [REFERENCES, ACADEMIC]);
    }

    fn hit(doc_id: &str, text: &str) -> SearchHit {
        SearchHit {
            store: ACADEMIC.into(),
            doc_id: doc_id.into(),
            title: None,
            authors: vec![],
            year: None,
            doi: None,
            chunk_text: text.into(),
            score: 0.0,
            line_start: 0,
            line_end: 0,
            page: None,
            category: None,
        }
    }

    #[tokio::test]
    async fn hits_are_returned_in_the_order_the_store_ranked_them() {
        // Ranking is the store's: the server adds no ordering of its own,
        // whatever a hit's text looks like.
        let h = harness().await;
        h.store.state.lock().canned_hits = vec![
            hit("references", BIBLIOGRAPHY),
            hit("prose", "The hippocampus generates new neurons."),
        ];
        let ids: Vec<String> = h
            .state
            .search(search_req("q", Some(10), None))
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.doc_id)
            .collect();
        assert_eq!(ids, ["references", "prose"]);
    }

    #[tokio::test]
    async fn list_docs_limit_is_bounded_and_never_silently_truncated() {
        let h = harness().await;
        for d in ["a", "b", "c"] {
            h.store.seed(ACADEMIC, d, 1);
        }
        for refused in [MAX_DOC_LIST_LIMIT + 1, u64::MAX, 0] {
            assert_eq!(
                kind(h.state.list_docs(Some(refused), IN_ACADEMIC).await),
                ErrorKind::BadRequest,
                "{refused}"
            );
        }

        let all = h.state.list_docs(None, IN_ACADEMIC).await.unwrap();
        assert_eq!(all.doc_ids, ["a", "b", "c"]);
        assert!(!all.truncated);

        let some = h.state.list_docs(Some(2), IN_ACADEMIC).await.unwrap();
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
        h.store.seed(ACADEMIC, "a", 3);
        h.store.seed(ACADEMIC, "b", 2);
        h.store.seed(ABSTRACTS, "c", 7);
        let s = h.state.status(IN_ACADEMIC).await.unwrap();
        assert_eq!((s.records_count, s.documents_count), (5, 2));
        assert!(!s.documents_count_truncated);
        assert_eq!(s.embed_model, OLLAMA_EMBED_MODEL);
        assert_eq!(s.compute_device, "cuda");
        assert_eq!(s.store, ACADEMIC);
    }

    #[tokio::test]
    async fn a_documents_chunks_are_counted_with_its_reference_lists() {
        // A caller that compares a document's chunk count with what an
        // index reported must see the same number, wherever the chunks
        // were filed.
        let h = harness().await;
        h.store.seed(ACADEMIC, "a", 3);
        h.store.seed(REFERENCES, "a", 2);
        h.store.seed(REFERENCES, "only_references", 1);
        h.store.seed(ABSTRACTS, "a", 1);

        let s = h.state.status(IN_ACADEMIC).await.unwrap();
        assert_eq!((s.records_count, s.documents_count), (6, 2));
        assert_eq!(
            h.state.doc_exists("a", IN_ACADEMIC).await.unwrap(),
            (true, 5)
        );
        assert_eq!(
            h.state
                .doc_exists("only_references", IN_ACADEMIC)
                .await
                .unwrap(),
            (true, 1)
        );
        assert_eq!(
            h.state.list_docs(None, IN_ACADEMIC).await.unwrap().doc_ids,
            ["a", "only_references"]
        );

        // A store with no reference store counts only itself.
        let s = h.state.status(Some(ABSTRACTS)).await.unwrap();
        assert_eq!((s.records_count, s.documents_count), (1, 1));
    }

    #[tokio::test]
    async fn status_reports_a_cpu_embedder_rather_than_hiding_it() {
        let h = harness().await;
        *h.ollama.mode.lock() = OllamaMode::Cpu;
        assert_eq!(
            h.state.status(IN_ACADEMIC).await.unwrap().compute_device,
            "cpu"
        );
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
        assert_eq!(ok.embed_model, OLLAMA_EMBED_MODEL);
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

        let status = h.state.status(IN_ACADEMIC).await.unwrap();
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
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), IN_ACADEMIC))
            .unwrap();
        assert_eq!(kind(h.state.run_index(job).await), ErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn indexing_is_refused_while_the_embedding_model_is_off_the_gpu() {
        let h = harness().await;
        for mode in [OllamaMode::Cpu, OllamaMode::Partial, OllamaMode::Down] {
            *h.ollama.mode.lock() = mode;
            let job = h
                .state
                .prepare_index(index_req(Some("d.md"), Some(&prose(6)), IN_ACADEMIC))
                .unwrap();
            assert_eq!(
                kind(h.state.run_index(job).await),
                ErrorKind::Unavailable,
                "{mode:?}"
            );
        }
        assert_eq!(h.store.ops(), [], "nothing may be written");
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
    }

    // ── reference lists ────────────────────────────────────────────────

    /// Three pages, one chunk each: prose, a reference list, prose.
    fn paper_with_a_reference_list() -> String {
        let page = prose(3);
        format!("{page}{PAGE_SEPARATOR}{BIBLIOGRAPHY}{PAGE_SEPARATOR}{page}")
    }

    async fn index_paper_into(h: &Harness, store: &str) -> u32 {
        let job = h
            .state
            .prepare_index(index_req(
                Some("d.md"),
                Some(&paper_with_a_reference_list()),
                Some(store),
            ))
            .unwrap();
        h.state.run_index(job).await.unwrap().chunks_indexed
    }

    #[tokio::test]
    async fn a_papers_reference_list_is_filed_in_the_reference_store() {
        let h = harness().await;
        assert_eq!(index_paper_into(&h, ACADEMIC).await, 3);
        assert_eq!(h.store.stored(ACADEMIC, "d"), [0, 2]);
        assert_eq!(h.store.stored(REFERENCES, "d"), [1]);
        assert_eq!(
            h.state.doc_exists("d", IN_ACADEMIC).await.unwrap(),
            (true, 3),
            "the document's chunk count must match what indexing reported"
        );
    }

    #[tokio::test]
    async fn a_store_without_a_reference_store_keeps_every_chunk() {
        let h = harness().await;
        assert_eq!(index_paper_into(&h, PERSONAL).await, 3);
        assert_eq!(h.store.stored(PERSONAL, "d"), [0, 1, 2]);
        assert!(h.store.stored(REFERENCES, "d").is_empty());
    }

    // ── per-document operations ────────────────────────────────────────

    #[tokio::test]
    async fn delete_reports_how_many_chunks_existed_and_removes_them() {
        let h = harness().await;
        h.store.seed(ACADEMIC, "d", 4);
        assert_eq!(h.state.delete_doc("d", IN_ACADEMIC).await.unwrap(), 4);
        assert!(h.store.stored(ACADEMIC, "d").is_empty());
        assert_eq!(h.state.delete_doc("d", IN_ACADEMIC).await.unwrap(), 0);
        assert_eq!(
            h.state.doc_exists("d", IN_ACADEMIC).await.unwrap(),
            (false, 0)
        );
    }

    #[tokio::test]
    async fn deleting_a_document_deletes_its_reference_lists_and_leaves_its_abstract() {
        // The abstract is written from the catalog and stamped there; it
        // is deleted by the caller that deletes the catalog row.
        let h = harness().await;
        h.store.seed(ACADEMIC, "d", 4);
        h.store.seed(REFERENCES, "d", 2);
        h.store.seed(ABSTRACTS, "d", 1);
        h.store.seed(REFERENCES, "other", 3);

        assert_eq!(h.state.delete_doc("d", IN_ACADEMIC).await.unwrap(), 6);
        assert!(h.store.stored(ACADEMIC, "d").is_empty());
        assert!(h.store.stored(REFERENCES, "d").is_empty());
        assert_eq!(h.store.stored(ABSTRACTS, "d").len(), 1);
        assert_eq!(h.store.stored(REFERENCES, "other").len(), 3);

        assert_eq!(h.state.delete_doc("d", Some(ABSTRACTS)).await.unwrap(), 1);
        assert!(h.store.stored(ABSTRACTS, "d").is_empty());
    }

    #[tokio::test]
    async fn doc_exists_counts_the_documents_own_chunks() {
        let h = harness().await;
        h.store.seed(ACADEMIC, "d", 4);
        h.store.seed(ACADEMIC, "other", 9);
        assert_eq!(
            h.state.doc_exists("d", IN_ACADEMIC).await.unwrap(),
            (true, 4)
        );
    }

    #[tokio::test]
    async fn doc_ids_that_could_traverse_are_rejected() {
        let h = harness().await;
        for bad in ["..", ".", "a/b", "a\\b"] {
            assert_eq!(
                kind(h.state.doc_exists(bad, IN_ACADEMIC).await),
                ErrorKind::BadRequest,
                "{bad:?}"
            );
            assert_eq!(
                kind(h.state.delete_doc(bad, IN_ACADEMIC).await),
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
            .prepare_index(index_req(Some("d.md"), Some(&prose(6)), IN_ACADEMIC))
            .unwrap();
        let err = h.state.run_index(job).await.expect_err("must fail");
        assert_eq!(err.kind, ErrorKind::Panicked);
        assert!(
            err.message.starts_with(PANIC_CODE) && err.message.contains("d"),
            "{err}"
        );
        assert_eq!(h.state.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(h.store.ops(), [], "nothing written");
    }
}
