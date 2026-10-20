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
use crate::fusion;
use crate::pipeline::{self, ContentProfile, IndexJob};
use crate::reference_list;
use crate::store::{SearchFilter, VectorStore};
use crate::types::ScrubReport;

/// How long `/readiness` waits for Qdrant before reporting not-ready.
const READINESS_QDRANT_TIMEOUT: Duration = Duration::from_secs(2);

/// Hits returned when a search request names no `limit`.
const DEFAULT_SEARCH_LIMIT: u64 = 10;

/// How many candidates the lexical channel contributes to a fusion.
///
/// Fusion ranks, so the pool has to be deeper than the answer: a chunk
/// that only the lexical channel likes must still be in the pool to be
/// ranked at all. Ten times the default page is the same order of
/// magnitude our sibling crate uses (myelin's `recall` asks the lexical
/// channel for 50 and returns 6).
const LEXICAL_CANDIDATES: u64 = 100;

/// Longest search query accepted, in bytes. The model reads at most
/// `embedding.max_length` tokens (8192 at most, well under this even for
/// 3-byte-per-token scripts); without a bound a single request could hand
/// the tokenizer megabytes while holding an embedder slot, stalling every
/// document being indexed behind it.
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

        let embeddings = self.embedder.embed_batch(vec![req.query.clone()]).await?;
        let query_vector = embeddings
            .into_iter()
            .next()
            .map(|e| e.dense)
            .ok_or_else(|| DistillError::Embedding("embedder returned no vector".into()))?;

        // Two channels, one ranking. The dense channel finds passages that
        // *mean* what the query means; the lexical channel finds passages
        // that *say* the query's words. Neither subsumes the other: a dense
        // embedder matches "thyroid" generically and loses "Hashimoto",
        // while a keyword match cannot answer "how does attention work".
        // Their ranks are fused, not their scores (see `crate::fusion`).
        // The candidate pool is the page. Measured: a wider pool
        // (50 candidates for a 10-result page) changes the ranking
        // enough to lose an expected passage — for "Dawson and Krebs"
        // the extra candidates are 137 chunks about a botanist named
        // Dawson, and the one that mentions Dawson and Krebs falls out
        // of the page.
        let pool = limit;
        let dense = self
            .store
            .search(&collection, query_vector, pool, &filter)
            .await?;

        // The lexical channel is asked for more candidates than the caller
        // wants: fusion works on ranks, so a candidate that only the
        // lexical channel likes still has to be in the pool to be fused.
        // The lexical channel keeps a deep pool (as in rc.365). Its
        // candidate sets are conjunctions of the query's terms, and the
        // deepest pool is what makes the right set win: for "Dawson and
        // Krebs" the set "dawson AND krebs" holds two chunks and the set
        // "dawson AND tricarboxylic" holds one, and the two rarest terms
        // decide which is asked for. A shallower pool changes that
        // choice, and with it the ranking.
        let lexical = self
            .store
            .lexical_search(&collection, &req.query, LEXICAL_CANDIDATES, &filter)
            .await?;

        if lexical.is_empty() {
            // No lexical candidate: the query has no term the text index
            // can match (or the index is not there). The dense ranking is
            // the whole answer — a query with no words to match has one
            // channel, not two. This is not a degraded fallback: it is the
            // complete result for a query the lexical channel cannot
            // speak to.
            let mut hits: Vec<SearchHit> = dense.into_iter().map(|(_, hit)| hit).collect();
            demote_reference_lists(&mut hits, limit);
            return Ok(hits);
        }

        Ok(fuse_channels(&dense, &lexical, limit))
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

/// Fuse the dense and lexical rankings into one result list.
///
/// The two channels are combined by rank, not by score (see
/// [`crate::fusion`]): a cosine similarity and a text-match count are not
/// on the same scale, and the one that produced the larger numbers would
/// otherwise win every comparison.
///
/// The returned score is the fused RRF score, so it is comparable between
/// hits of one search and meaningless across searches — the same property
/// the dense channel's cosine score has.
fn fuse_channels(
    dense: &[(String, SearchHit)],
    lexical: &[(String, SearchHit)],
    limit: u64,
) -> Vec<SearchHit> {
    let dense_list = fusion::RankedList::new(dense.iter().map(|(id, _)| id.clone()).collect());
    let lexical_list = fusion::RankedList::new(lexical.iter().map(|(id, _)| id.clone()).collect());
    let fused = fusion::rrf(&[dense_list, lexical_list], fusion::DEFAULT_RRF_K);

    // A hit's payload is needed whichever channel surfaced it. The dense
    // channel is the one that carries a score, so its copy of the payload
    // wins; a hit only the lexical channel found has no dense copy.
    let mut by_id: std::collections::HashMap<&str, &SearchHit> =
        std::collections::HashMap::with_capacity(dense.len() + lexical.len());
    for (id, hit) in dense.iter().chain(lexical.iter()) {
        by_id.entry(id.as_str()).or_insert(hit);
    }

    // The whole fused list is built, not just the page: the reference
    // list demotion below needs prose to promote, and prose that was
    // fused at rank 12 has to still be here for it to be promoted into
    // the page. Truncating first would leave the demotion nothing to
    // work with at small limits.
    let mut hits: Vec<SearchHit> = fused
        .into_iter()
        .filter_map(|(id, score)| {
            let hit = *by_id.get(id.as_str())?;
            let mut hit = hit.clone();
            hit.score = score;
            Some(hit)
        })
        .collect();
    demote_reference_lists(&mut hits, limit);
    hits
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
        // The limit is the number of results the caller gets. The store
        // is asked for a deeper pool (the fusion needs candidates to
        // rank, and the reference-list demotion needs prose to
        // promote), so the pool size is not the limit.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = (0..100).map(|i| hit(&format!("d{i}"), "text")).collect();
            st.canned_lexical_hits = (0..100).map(|i| hit(&format!("l{i}"), "text")).collect();
        }
        h.state.search(search_req("q", None, None)).await.unwrap();
        h.state
            .search(search_req("q", Some(25), None))
            .await
            .unwrap();
        h.state
            .search(search_req("q", Some(u64::MAX), None))
            .await
            .unwrap();
        let limits: Vec<u64> = {
            let st = h.store.state.lock();
            st.searches.iter().map(|s| s.1).collect()
        };
        // The pool is the page: a wider pool changes the ranking enough
        // to lose an expected passage.
        assert_eq!(limits, [10, 25, MAX_SEARCH_LIMIT]);

        // The page the caller gets is the limit.
        assert_eq!(
            h.state
                .search(search_req("q", None, None))
                .await
                .unwrap()
                .len(),
            DEFAULT_SEARCH_LIMIT as usize
        );
        assert_eq!(
            h.state
                .search(search_req("q", Some(25), None))
                .await
                .unwrap()
                .len(),
            25
        );
        assert_eq!(
            h.state
                .search(search_req("q", Some(u64::MAX), None))
                .await
                .unwrap()
                .len(),
            MAX_SEARCH_LIMIT as usize
        );

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
    async fn an_oversized_search_query_is_rejected_before_it_reaches_the_embedder() {
        let h = harness();
        let at_cap = "q".repeat(MAX_SEARCH_QUERY_BYTES);
        h.state
            .search(search_req(&at_cap, None, None))
            .await
            .unwrap();
        assert_eq!(h.embedder.calls(), 1);

        let over = "q".repeat(MAX_SEARCH_QUERY_BYTES + 1);
        assert_eq!(
            kind(h.state.search(search_req(&over, None, None)).await),
            ErrorKind::BadRequest
        );
        assert_eq!(h.embedder.calls(), 1, "the oversized query was embedded");
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

    // ── hybrid (dense + lexical) search ────────────────────────────

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

    #[tokio::test]
    async fn a_hit_only_the_lexical_channel_found_is_in_the_result() {
        // The dense channel missed the passage that actually contains the
        // term; the lexical channel found it. The fused result must
        // contain it — that is the whole point of the second channel.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![hit("dense-only", "a passage about something else")];
            st.canned_lexical_hits = vec![hit("lexical-only", "Hashimoto thyroiditis")];
        }
        let hits = h
            .state
            .search(search_req("Hashimoto thyroiditis", Some(10), None))
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|x| x.doc_id.as_str()).collect();
        assert!(
            ids.contains(&"lexical-only"),
            "the lexical channel's hit is missing: {ids:?}"
        );
        assert!(
            ids.contains(&"dense-only"),
            "the dense hit is missing: {ids:?}"
        );
    }

    #[tokio::test]
    async fn a_hit_both_channels_found_outranks_one_only_the_dense_channel_found() {
        let h = harness();
        {
            let mut st = h.store.state.lock();
            // The fake store uses the doc id as the point id, so this is
            // one point that both channels found.
            st.canned_hits = vec![hit("both-channels", "in both")];
            st.canned_lexical_hits = vec![hit("both-channels", "in both")];
        }
        let hits = h
            .state
            .search(search_req("both", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "the same point must appear once");
        assert_eq!(hits[0].doc_id, "both-channels");
    }

    #[tokio::test]
    async fn the_result_is_capped_at_the_requested_limit() {
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = (0..5).map(|i| hit(&format!("d{i}"), "dense")).collect();
            st.canned_lexical_hits = (0..5).map(|i| hit(&format!("l{i}"), "lexical")).collect();
        }
        let hits = h
            .state
            .search(search_req("q", Some(3), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn a_query_with_no_lexical_match_keeps_the_dense_ranking() {
        // No term the text index can match: the dense channel is the whole
        // answer. This is the complete result for such a query, not a
        // degraded one.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![hit("d0", "dense")];
        }
        let hits = h
            .state
            .search(search_req("q", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "d0");
    }

    #[tokio::test]
    async fn the_lexical_channel_is_asked_for_a_deep_pool() {
        // The lexical channel's candidate sets are conjunctions of the
        // query's terms, and the deepest pool is what makes the right
        // set win: for "Dawson and Krebs" the set "dawson AND krebs"
        // holds two chunks and "dawson AND tricarboxylic" holds one.
        // A shallower pool changes that choice, and with it the ranking.
        let h = harness();
        h.state
            .search(search_req("Hashimoto", Some(10), None))
            .await
            .unwrap();
        let st = h.store.state.lock();
        assert_eq!(st.lexical_searches.len(), 1);
        assert_eq!(st.lexical_searches[0].0, "Hashimoto");
        assert!(
            st.lexical_searches[0].1 > 10,
            "the lexical pool must be deeper than the page"
        );
    }

    #[tokio::test]
    async fn the_fused_score_is_the_rrf_score_not_a_cosine() {
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![hit("both-channels", "dense")];
            st.canned_lexical_hits = vec![hit("both-channels", "both")];
        }
        let hits = h
            .state
            .search(search_req("q", Some(10), None))
            .await
            .unwrap();
        // One hit, first in both lists: 1/(1+1) + 1/(1+1) = 1.0.
        assert!(
            (hits[0].score - 1.0).abs() < 1e-6,
            "score {}",
            hits[0].score
        );
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
        // The dense channel ranks a bibliography entry at the top for a
        // topic query, because a citation to work about the topic *is*
        // about the topic. When prose outranks it, the passage that
        // discusses the topic must come first.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            // The prose heads the dense list, so it outranks the
            // reference list. The lexical channel also finds the prose,
            // which lifts it further.
            st.canned_hits = vec![
                hit(
                    "prose",
                    "The hippocampus generates new neurons throughout adulthood.",
                ),
                bibliography_chunk("references"),
            ];
            st.canned_lexical_hits = vec![hit(
                "prose",
                "The hippocampus generates new neurons throughout adulthood.",
            )];
        }
        let hits = h
            .state
            .search(search_req("hippocampus neurogenesis", Some(10), None))
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|x| x.doc_id.as_str()).collect();
        assert_eq!(ids, vec!["prose", "references"], "order was {ids:?}");
    }

    #[tokio::test]
    async fn a_demoted_reference_list_is_still_returned() {
        // A user asking for a paper's bibliography wants it: the chunk is
        // demoted, not dropped.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![
                hit(
                    "prose",
                    "The hippocampus generates new neurons throughout adulthood.",
                ),
                bibliography_chunk("references"),
            ];
            // The lexical channel also finds the prose, so the fused
            // path runs and the hits carry an RRF score.
            st.canned_lexical_hits = vec![hit(
                "prose",
                "The hippocampus generates new neurons throughout adulthood.",
            )];
        }
        let hits = h
            .state
            .search(search_req("hippocampus neurogenesis", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "the reference list was dropped, not demoted");
        assert_eq!(hits[1].doc_id, "references");
        // Its score survives: it is still ranked, just lower.
        assert!(hits[1].score > 0.0, "the demoted hit lost its score");
    }

    #[tokio::test]
    async fn a_mixed_chunk_is_not_demoted() {
        // A chunk that cites a work and then discusses it is useful, and
        // must not be treated as a reference list.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits =
                vec![
                hit(
                    "mixed",
                    "Neurogenesis was first reported by Altman (1962) in rodents, and the rate of \
                     new neuron production in the adult hippocampus remains contested to this day.",
                ),
                hit("other", "Something else entirely about a different subject."),
            ];
        }
        let hits = h
            .state
            .search(search_req("hippocampus neurogenesis", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits[0].doc_id, "mixed", "a mixed chunk was demoted");
    }

    #[tokio::test]
    async fn the_demotion_does_not_change_the_number_of_results() {
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = (0..4)
                .map(|i| bibliography_chunk(&format!("refs{i}")))
                .collect();
        }
        let hits = h
            .state
            .search(search_req("q", Some(10), None))
            .await
            .unwrap();
        assert_eq!(hits.len(), 4, "results were dropped");
    }

    #[tokio::test]
    async fn a_small_limit_still_demotes_a_reference_list() {
        // The demotion needs prose to promote, and the prose sits below
        // the page. A search for three results must still ask the
        // channels for a deeper pool, or the demotion has nothing to
        // promote and the reference list stays at the top.
        //
        // The fake store's ORDER is the ranking (the fusion assigns the
        // scores), so the prose comes first in the dense list and the
        // reference list comes after it.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![
                hit(
                    "prose-1",
                    "The split brain was studied by Sperry and Gazzaniga.",
                ),
                hit(
                    "prose-2",
                    "A second prose passage about hemispheric specialization.",
                ),
                bibliography_chunk("references"),
            ];
            st.canned_lexical_hits = vec![hit(
                "prose-1",
                "The split brain was studied by Sperry and Gazzaniga.",
            )];
        }
        let hits = h
            .state
            .search(search_req("split brain", Some(3), None))
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|x| x.doc_id.as_str()).collect();
        assert_eq!(ids[0], "prose-1", "the prose did not come first: {ids:?}");
        assert!(
            ids.contains(&"references"),
            "the reference list was dropped, not demoted: {ids:?}"
        );
    }

    #[tokio::test]
    async fn a_reference_list_leads_only_when_there_is_no_prose() {
        // Sometimes the only chunk in the corpus that mentions the term
        // IS a reference list — for "Dawson and Krebs" it is the
        // citation of Dawson's paper, and the corpus holds 137 chunks
        // about a botanist named Dawson and no paper about the Krebs
        // cycle by that name. With nothing else to show, the reference
        // list is the answer and keeps its place.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![bibliography_chunk("references")];
        }
        let hits = h
            .state
            .search(search_req("q", Some(3), None))
            .await
            .unwrap();
        assert_eq!(
            hits[0].doc_id, "references",
            "the only answer was demoted below nothing"
        );
    }

    #[tokio::test]
    async fn prose_leads_even_when_a_reference_list_outranks_it() {
        // The reference list outranks the prose — and the user asked a
        // question, not for a bibliography. The passage that discusses
        // the topic is the answer.
        let h = harness();
        {
            let mut st = h.store.state.lock();
            st.canned_hits = vec![
                bibliography_chunk("references"),
                hit("prose", "Something about the subject matter."),
            ];
        }
        let hits = h
            .state
            .search(search_req("q", Some(3), None))
            .await
            .unwrap();
        assert_eq!(
            hits[0].doc_id, "prose",
            "the reference list was not demoted below the prose"
        );
    }

    #[tokio::test]
    async fn the_dense_pool_is_the_page() {
        // A wider dense pool changes the ranking enough to lose an
        // expected passage: for "Dawson and Krebs", the extra candidates
        // are 137 chunks about a botanist named Dawson, and the chunk
        // that mentions Dawson and Krebs falls out of the page. So the
        // dense pool is exactly what the caller asked for.
        let h = harness();
        h.state
            .search(search_req("q", Some(3), None))
            .await
            .unwrap();
        let st = h.store.state.lock();
        let (_, dense_limit, _) = &st.searches[0];
        assert_eq!(*dense_limit, 3, "the dense pool was widened");
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
