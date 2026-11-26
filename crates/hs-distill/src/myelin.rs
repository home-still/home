//! [`DocumentStore`] over a myelin MCP server (`myelin-mcp`, streamable HTTP).
//!
//! # Mapping
//!
//! | home-still                         | myelin                                 |
//! |------------------------------------|----------------------------------------|
//! | collection (`academic_papers`, …)  | `tenant`                               |
//! | `doc_id`                           | `namespace`                            |
//! | one chunk                          | one episode (`speaker: "document"`)    |
//! | chunk metadata (title, year, page) | the turn's caller `meta`               |
//!
//! Papers are stored verbatim: `extract` is off, so myelin embeds the chunk
//! text and keeps it as the record's value; nothing is summarised or merged.
//!
//! # Generations
//!
//! myelin never updates a record, so a re-index writes a new *generation*
//! and then deletes the older ones. A generation is named by a hash of the
//! chunks it holds, so indexing identical content again is a no-op on the
//! myelin side (it already holds those episodes) and the delete that follows
//! has nothing to remove.
//!
//! # Sessions
//!
//! Every operation opens its own MCP session and closes it. There is no
//! long-lived session, so a myelin restart needs no reconnect logic.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use hs_common::auth::client::AuthedHttp;
use hs_common::mcp_client::McpClient;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::client::SearchHit;
use crate::error::DistillError;
use crate::store::{DocumentStore, SearchFilter, YearFilter};
use crate::types::Chunk;

/// Longest one myelin call may take: a 2,000-chunk book is embedded inside
/// the single `observe` that writes it.
const MYELIN_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Longest connecting to myelin may take.
const MYELIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The `agent` every write carries.
const AGENT: &str = "hs-distill";

/// The speaker of a stored document chunk; myelin renders a recalled episode
/// as `"{speaker}: {text}"`.
const SPEAKER: &str = "document";

/// Tokens of recall budget per requested hit. A chunk is ~1,000 tokens plus
/// its header; the budget must never be what cuts a page short.
const BUDGET_TOKENS_PER_HIT: u64 = 2048;

pub struct MyelinStore {
    http: reqwest::Client,
    endpoint: String,
    /// myelin keeps one SQLite ledger; writes are serialized here so two
    /// documents never contend for it.
    write_lock: tokio::sync::Mutex<()>,
}

impl MyelinStore {
    /// `endpoint` is the MCP URL (`http://127.0.0.1:7462/mcp`).
    pub fn new(endpoint: &str) -> Result<Self, DistillError> {
        crate::config::parse_myelin_url(endpoint)?;
        let http = hs_common::http::client_builder()
            .connect_timeout(MYELIN_CONNECT_TIMEOUT)
            .timeout(MYELIN_CALL_TIMEOUT)
            .build()
            .map_err(|e| DistillError::Config(format!("building the myelin HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoint: endpoint.to_string(),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Open a session. The backend token is deliberately not sent: it is the
    /// home-still shared secret, and myelin is not a home-still service.
    async fn session(&self) -> Result<McpClient, DistillError> {
        McpClient::connect(
            AuthedHttp::plain_with_backend_token(self.http.clone(), None),
            self.endpoint.clone(),
        )
        .await
        .map_err(|e| DistillError::Backend(format!("myelin unreachable: {e:#}")))
    }

    /// One tool call in its own session.
    async fn call(&self, tool: &str, arguments: Value) -> Result<Value, DistillError> {
        let session = self.session().await?;
        let result = session.call_tool(tool, arguments).await;
        session.close_logged().await;
        result.map_err(|e| DistillError::Backend(format!("myelin {tool}: {e:#}")))
    }
}

fn decode<T: serde::de::DeserializeOwned>(tool: &str, value: Value) -> Result<T, DistillError> {
    serde_json::from_value(value).map_err(|e| {
        DistillError::Backend(format!("myelin {tool} returned an unusable result: {e}"))
    })
}

// ── Chunk metadata ─────────────────────────────────────────────────

/// Coerce a `publication_date` string into a sane integer year for the
/// record's metadata. The first 4 chars must parse as i64 within
/// `[1900, current_year+1]`; anything outside that band (e.g. `"2041-..."`
/// from a citation regex hit, or a corrupt catalog row) becomes `None`, so
/// the field is null rather than misleadingly precise. `+1` tolerates
/// next-year preprints.
pub(crate) fn sanitize_year(publication_date: Option<&str>) -> Option<i64> {
    use chrono::Datelike;
    let year = publication_date?.get(..4)?.parse::<i64>().ok()?;
    let now_year = chrono::Utc::now().year() as i64;
    (1900..=now_year + 1).contains(&year).then_some(year)
}

/// The caller metadata stored with one chunk's record. Everything a search
/// hit needs besides the text itself.
fn chunk_meta(chunk: &Chunk) -> Result<Map<String, Value>, DistillError> {
    // myelin stores `text` (header included, so it is embedded); a hit's
    // `chunk_text` is the part after the header.
    if !chunk.text.ends_with(&chunk.raw_text) {
        return Err(DistillError::InvalidInput(format!(
            "chunk {} of {} does not end with its raw text; the header offset cannot be derived",
            chunk.chunk_index, chunk.doc_id
        )));
    }
    let meta = &chunk.meta;
    let value = json!({
        "doc_id": chunk.doc_id,
        "chunk_index": chunk.chunk_index,
        "total_chunks": chunk.total_chunks,
        "raw_offset": chunk.text.len() - chunk.raw_text.len(),
        "title": meta.title,
        "authors": meta.authors,
        "doi": meta.doi,
        "year": sanitize_year(meta.publication_date.as_deref()),
        "topics": meta.topics,
        "keywords": meta.keywords,
        "pdf_path": meta.pdf_path,
        "markdown_path": meta.markdown_path,
        "line_start": chunk.span.line_start as i64,
        "line_end": chunk.span.line_end as i64,
        "page": chunk.page.map(|p| p as i64),
        "cited_by_count": meta.cited_by_count,
        "category": meta.category,
        "original_format": meta.original_format,
        "ingested_at": meta.ingested_at,
    });
    match value {
        Value::Object(map) => Ok(map),
        _ => unreachable!("json! of an object literal is an object"),
    }
}

/// What a stored chunk's metadata says about a search hit.
#[derive(Deserialize)]
struct ChunkMeta {
    doc_id: String,
    raw_offset: usize,
    title: Option<String>,
    authors: Option<Vec<String>>,
    year: Option<u64>,
    doi: Option<String>,
    pdf_path: Option<String>,
    line_start: Option<usize>,
    line_end: Option<usize>,
    page: Option<usize>,
    category: Option<String>,
}

// ── Wire shapes of the myelin tools ────────────────────────────────

#[derive(Deserialize)]
struct ObserveResult {
    record_ids: Vec<String>,
    quarantined: u64,
    rejected: u64,
    adjudicated_out: u64,
}

#[derive(Deserialize)]
struct ForgetResult {
    affected: u64,
}

#[derive(Deserialize)]
struct RecallResult {
    items: Vec<RecallItem>,
    hits: Vec<RecallHit>,
}

#[derive(Deserialize)]
struct RecallItem {
    value: String,
}

#[derive(Deserialize)]
struct RecallHit {
    record_id: String,
    score: f32,
    meta: Option<Value>,
}

#[derive(Deserialize)]
struct NamespacesResult {
    namespaces: Vec<NamespaceRow>,
}

#[derive(Deserialize)]
struct NamespaceRow {
    namespace: String,
    records: u64,
}

// ── Writing ────────────────────────────────────────────────────────

/// Name of the generation `chunks` form: a hash of every chunk's text and
/// metadata, in chunk order. The same content always yields the same name,
/// so re-indexing it writes the same episodes.
fn generation(chunks: &[Chunk], metas: &[Map<String, Value>]) -> Result<String, DistillError> {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for (chunk, meta) in chunks.iter().zip(metas) {
        hasher.update(chunk.text.as_bytes());
        hasher.update(&[0x1f]);
        hasher.update(serde_json::to_string(meta)?.as_bytes());
        hasher.update(&[0x1e]);
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

#[async_trait]
impl DocumentStore for MyelinStore {
    async fn health(&self) -> Result<String, DistillError> {
        let session = self.session().await?;
        let info = session.server_info().cloned();
        session.close_logged().await;
        let info = info.ok_or_else(|| {
            DistillError::Backend("myelin's initialize result carried no serverInfo".into())
        })?;
        let name = info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let version = info.get("version").and_then(Value::as_str).unwrap_or("?");
        Ok(format!("{name} {version}"))
    }

    async fn write_document(
        &self,
        collection: &str,
        doc_id: &str,
        chunks: &[Chunk],
    ) -> Result<Vec<String>, DistillError> {
        if chunks.is_empty() {
            return Err(DistillError::InvalidInput(format!(
                "{doc_id}: a document write needs at least one chunk"
            )));
        }
        let metas = chunks
            .iter()
            .map(chunk_meta)
            .collect::<Result<Vec<_>, _>>()?;
        let generation = generation(chunks, &metas)?;
        let unit = format!("{doc_id}@{generation}");
        let turns: Vec<Value> = chunks
            .iter()
            .zip(metas)
            .enumerate()
            .map(|(i, (chunk, meta))| {
                let chunk_unit = format!("{unit}#{i:05}");
                json!({
                    "speaker": SPEAKER,
                    "text": chunk.text,
                    "source": chunk_unit,
                    "unit": chunk_unit,
                    "meta": meta,
                })
            })
            .collect();

        let _write = self.write_lock.lock().await;
        let result = self
            .call(
                "observe",
                json!({
                    "tenant": collection,
                    "namespace": doc_id,
                    "agent": AGENT,
                    "unit": unit,
                    "extract": false,
                    "turns": turns,
                }),
            )
            .await?;
        let observed: ObserveResult = decode("observe", result)?;
        if observed.quarantined != 0 || observed.rejected != 0 || observed.adjudicated_out != 0 {
            return Err(DistillError::Backend(format!(
                "myelin did not store {doc_id} as written ({} quarantined, {} rejected, {} \
                 adjudicated out of {} chunks)",
                observed.quarantined,
                observed.rejected,
                observed.adjudicated_out,
                chunks.len()
            )));
        }
        if observed.record_ids.len() != chunks.len() {
            return Err(DistillError::Backend(format!(
                "myelin returned {} record ids for the {} chunks of {doc_id}",
                observed.record_ids.len(),
                chunks.len()
            )));
        }
        Ok(observed.record_ids)
    }

    async fn forget_document(
        &self,
        collection: &str,
        doc_id: &str,
        keep: &[String],
    ) -> Result<u64, DistillError> {
        let _write = self.write_lock.lock().await;
        let result = self
            .call(
                "forget_scope",
                json!({
                    "tenant": collection,
                    "namespace": doc_id,
                    "keep": keep,
                    "confirm": true,
                    "actor": AGENT,
                }),
            )
            .await?;
        Ok(decode::<ForgetResult>("forget_scope", result)?.affected)
    }

    async fn search(
        &self,
        collection: &str,
        query: &str,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError> {
        let result = self
            .call(
                "recall",
                json!({
                    "query": query,
                    "tenant": collection,
                    "k": limit,
                    "budget_tokens": limit.saturating_mul(BUDGET_TOKENS_PER_HIT),
                    "dated": false,
                    "filter": filter_conditions(filter),
                }),
            )
            .await?;
        let recalled: RecallResult = decode("recall", result)?;
        if recalled.items.len() != recalled.hits.len() {
            return Err(DistillError::Backend(format!(
                "myelin recall returned {} items for {} hits",
                recalled.items.len(),
                recalled.hits.len()
            )));
        }

        let mut ranked: Vec<(String, SearchHit)> = Vec::with_capacity(recalled.hits.len());
        for (item, hit) in recalled.items.into_iter().zip(recalled.hits) {
            // A record without caller metadata is a synthetic view myelin
            // adds to a recall (a timeline, notes), not a stored chunk.
            let Some(meta) = hit.meta else { continue };
            let meta: ChunkMeta = decode("recall", meta)?;
            let text = item.value.strip_prefix("document: ").ok_or_else(|| {
                DistillError::Backend(format!(
                    "myelin recall returned record {} without the `document: ` prefix",
                    hit.record_id
                ))
            })?;
            let chunk_text = text.get(meta.raw_offset..).ok_or_else(|| {
                DistillError::Backend(format!(
                    "record {} is shorter than its stored header offset {}",
                    hit.record_id, meta.raw_offset
                ))
            })?;
            ranked.push((
                hit.record_id,
                SearchHit {
                    doc_id: meta.doc_id,
                    title: meta.title,
                    authors: meta.authors.unwrap_or_default(),
                    year: meta.year,
                    doi: meta.doi,
                    chunk_text: chunk_text.to_string(),
                    score: hit.score,
                    pdf_path: meta.pdf_path,
                    line_start: meta.line_start.unwrap_or(0),
                    line_end: meta.line_end.unwrap_or(0),
                    page: meta.page,
                    category: meta.category,
                },
            ));
        }
        // myelin orders a recall for a reader (best evidence at the edges);
        // a search result is ordered by score.
        ranked.sort_by(|(a_id, a), (b_id, b)| {
            b.score.total_cmp(&a.score).then_with(|| a_id.cmp(b_id))
        });
        ranked.truncate(limit as usize);
        Ok(ranked.into_iter().map(|(_, hit)| hit).collect())
    }

    async fn inventory(
        &self,
        collection: &str,
        doc_id: Option<&str>,
    ) -> Result<BTreeMap<String, u64>, DistillError> {
        let mut arguments = json!({ "tenant": collection });
        if let Some(doc_id) = doc_id {
            arguments["namespace"] = json!(doc_id);
        }
        let result = self.call("namespaces", arguments).await?;
        let rows: NamespacesResult = decode("namespaces", result)?;
        Ok(rows
            .namespaces
            .into_iter()
            .map(|row| (row.namespace, row.records))
            .collect())
    }
}

/// `filter` conditions of a myelin `recall` for a search filter.
fn filter_conditions(filter: &SearchFilter) -> Vec<Value> {
    let mut conditions = Vec::new();
    if let Some(year) = filter.year {
        let bound = |name: &str, y: i64| json!({"op": "range", "key": "year", name: y});
        conditions.push(match year {
            YearFilter::Eq(y) => json!({"op": "range", "key": "year", "gte": y, "lte": y}),
            YearFilter::Gt(y) => bound("gt", y),
            YearFilter::Gte(y) => bound("gte", y),
            YearFilter::Lt(y) => bound("lt", y),
            YearFilter::Lte(y) => bound("lte", y),
        });
    }
    if let Some(topic) = &filter.topic {
        conditions.push(json!({"op": "eq", "key": "topics", "value": topic}));
    }
    if let Some(category) = &filter.category {
        conditions.push(json!({"op": "eq", "key": "category", "value": category}));
    }
    conditions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{serve, FakeHttp, Reply};
    use crate::types::{ChunkSpan, DocumentMeta};
    use chrono::Datelike;

    // ── An MCP stand-in ────────────────────────────────────────────

    /// A myelin stand-in: answers the MCP handshake, then every `tools/call`
    /// with `tool(name, arguments)`.
    async fn stand_in(tool: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> FakeHttp {
        serve(move |req| {
            let sse = |frame: Value| format!("data: \n\nid: 0\nretry: 3000\n\ndata: {frame}\n\n");
            if req.request_line.starts_with("DELETE") {
                return Reply::Json(200, String::new());
            }
            let rpc: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
            match rpc["method"].as_str() {
                Some("initialize") => Reply::JsonWithHeader(
                    200,
                    sse(json!({"jsonrpc": "2.0", "id": 1, "result": {
                        "serverInfo": {"name": "myelin-mcp", "version": "0.4.2"}}})),
                    "mcp-session-id",
                    "sess-1",
                ),
                Some("notifications/initialized") => Reply::Json(202, String::new()),
                Some("tools/call") => {
                    let name = rpc["params"]["name"].as_str().unwrap_or_default();
                    let out = tool(name, &rpc["params"]["arguments"]);
                    Reply::Json(
                        200,
                        sse(json!({"jsonrpc": "2.0", "id": 99, "result": {
                            "content": [{"type": "text", "text": out.to_string()}],
                            "isError": false}})),
                    )
                }
                _ => Reply::Json(404, "no such method".into()),
            }
        })
        .await
    }

    /// Every `tools/call` the stand-in received: (tool, arguments).
    fn tool_calls(fake: &FakeHttp) -> Vec<(String, Value)> {
        fake.recorded()
            .iter()
            .filter_map(|r| {
                let rpc: Value = serde_json::from_str(&r.body).ok()?;
                (rpc["method"] == "tools/call").then(|| {
                    (
                        rpc["params"]["name"].as_str().unwrap().to_string(),
                        rpc["params"]["arguments"].clone(),
                    )
                })
            })
            .collect()
    }

    fn store(fake: &FakeHttp) -> MyelinStore {
        MyelinStore::new(&format!("{}/mcp", fake.url())).unwrap()
    }

    fn chunk(doc_id: &str, index: u32, total: u32, raw: &str) -> Chunk {
        Chunk {
            doc_id: doc_id.into(),
            chunk_index: index,
            total_chunks: total,
            text: format!("A Title > chunk {index}\n\n{raw}"),
            raw_text: raw.into(),
            span: ChunkSpan {
                line_start: 3,
                line_end: 9,
                char_start: 0,
                char_end: raw.len(),
            },
            page: Some(index as usize + 1),
            meta: DocumentMeta {
                doc_id: doc_id.into(),
                title: Some("A Title".into()),
                authors: vec!["A. Author".into()],
                doi: Some("10.1/x".into()),
                publication_date: Some("2021-03-04".into()),
                markdown_path: format!("markdown/xx/{doc_id}.md"),
                topics: vec!["autism".into()],
                ..Default::default()
            },
        }
    }

    fn observe_ok(ids: &[&str]) -> Value {
        json!({"record_ids": ids, "quarantined": 0, "rejected": 0, "adjudicated_out": 0})
    }

    // ── write_document ─────────────────────────────────────────────

    #[tokio::test]
    async fn a_write_is_verbatim_one_unit_per_chunk() {
        let fake = stand_in(|_, _| observe_ok(&["r0", "r1"])).await;
        let chunks = [
            chunk("doc", 0, 2, "first body"),
            chunk("doc", 1, 2, "second body"),
        ];
        let ids = store(&fake)
            .write_document("academic_papers", "doc", &chunks)
            .await
            .unwrap();
        assert_eq!(ids, ["r0", "r1"]);

        let calls = tool_calls(&fake);
        assert_eq!(calls.len(), 1);
        let (tool, args) = &calls[0];
        assert_eq!(tool, "observe");
        assert_eq!(args["tenant"], "academic_papers");
        assert_eq!(args["namespace"], "doc");
        assert_eq!(args["agent"], "hs-distill");
        assert_eq!(args["extract"], false, "papers are written verbatim");
        let turns = args["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        let generation = args["unit"].as_str().unwrap();
        assert!(generation.starts_with("doc@"), "{generation}");
        for (i, turn) in turns.iter().enumerate() {
            let unit = format!("{generation}#{i:05}");
            assert_eq!(turn["speaker"], "document");
            assert_eq!(turn["unit"], unit.as_str());
            assert_eq!(turn["source"], unit.as_str());
            assert_eq!(turn["text"], chunks[i].text.as_str());
            let meta = &turn["meta"];
            assert_eq!(meta["doc_id"], "doc");
            assert_eq!(meta["chunk_index"], i);
            assert_eq!(meta["total_chunks"], 2);
            assert_eq!(meta["year"], 2021);
            assert_eq!(meta["title"], "A Title");
            assert_eq!(meta["page"], i + 1);
            assert_eq!(meta["topics"], json!(["autism"]));
            assert_eq!(
                meta["raw_offset"],
                chunks[i].text.len() - chunks[i].raw_text.len()
            );
            assert!(meta.get("chunk_text").is_none(), "the text is the record");
        }
        // The session is closed.
        assert!(fake
            .recorded()
            .iter()
            .any(|r| r.request_line.starts_with("DELETE")));
    }

    #[tokio::test]
    async fn the_same_content_is_the_same_generation_and_other_content_is_not() {
        let fake = stand_in(|_, _| observe_ok(&["r0"])).await;
        let s = store(&fake);
        let a = [chunk("doc", 0, 1, "same body")];
        s.write_document("c", "doc", &a).await.unwrap();
        s.write_document("c", "doc", &a).await.unwrap();
        let b = [chunk("doc", 0, 1, "other body")];
        s.write_document("c", "doc", &b).await.unwrap();
        let calls = tool_calls(&fake);
        assert_eq!(calls[0].1["unit"], calls[1].1["unit"]);
        assert_eq!(calls[0].1["turns"], calls[1].1["turns"]);
        assert_ne!(calls[0].1["unit"], calls[2].1["unit"]);
    }

    #[tokio::test]
    async fn a_metadata_change_is_a_new_generation() {
        let fake = stand_in(|_, _| observe_ok(&["r0"])).await;
        let s = store(&fake);
        let a = chunk("doc", 0, 1, "body");
        let mut b = a.clone();
        b.meta.title = Some("A Better Title".into());
        // The title is part of the header, so keep the text equal and
        // change only the metadata.
        b.page = Some(40);
        s.write_document("c", "doc", &[a]).await.unwrap();
        s.write_document("c", "doc", &[b]).await.unwrap();
        let calls = tool_calls(&fake);
        assert_ne!(calls[0].1["unit"], calls[1].1["unit"]);
    }

    #[tokio::test]
    async fn a_write_whose_record_count_differs_from_the_chunk_count_fails() {
        let fake = stand_in(|_, _| observe_ok(&["only-one"])).await;
        let chunks = [chunk("doc", 0, 2, "a body"), chunk("doc", 1, 2, "b body")];
        let err = store(&fake)
            .write_document("c", "doc", &chunks)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DistillError::Backend(m) if m.contains("1 record ids") && m.contains("2 chunks")),
            "{err}"
        );
        assert_eq!(tool_calls(&fake).len(), 1, "a failed write is not retried");
    }

    #[tokio::test]
    async fn quarantined_rejected_or_adjudicated_chunks_fail_the_write() {
        for counter in ["quarantined", "rejected", "adjudicated_out"] {
            let mut reply = observe_ok(&["r0"]);
            reply[counter] = json!(1);
            let fake = stand_in(move |_, _| reply.clone()).await;
            let err = store(&fake)
                .write_document("c", "doc", &[chunk("doc", 0, 1, "body")])
                .await
                .unwrap_err();
            assert!(matches!(err, DistillError::Backend(_)), "{counter}: {err}");
        }
    }

    #[tokio::test]
    async fn a_chunk_whose_text_does_not_end_with_its_raw_text_is_invalid_input() {
        let fake = stand_in(|_, _| observe_ok(&["r0"])).await;
        let mut bad = chunk("doc", 0, 1, "body");
        bad.raw_text = "something else".into();
        let err = store(&fake)
            .write_document("c", "doc", &[bad])
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::InvalidInput(_)), "{err}");
        assert!(tool_calls(&fake).is_empty(), "nothing may reach myelin");

        let err = store(&fake)
            .write_document("c", "doc", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::InvalidInput(_)), "{err}");
    }

    #[tokio::test]
    async fn an_error_from_myelin_is_a_backend_error() {
        let fake = serve(|_| Reply::Json(503, "down".into())).await;
        let err = store(&fake)
            .write_document("c", "doc", &[chunk("doc", 0, 1, "body")])
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    #[test]
    fn a_bad_myelin_url_is_a_config_error() {
        for bad in ["", "not a url", "ftp://host/mcp"] {
            assert!(
                matches!(MyelinStore::new(bad), Err(DistillError::Config(_))),
                "{bad:?}"
            );
        }
    }

    // ── search ─────────────────────────────────────────────────────

    fn recall_reply(rows: &[(&str, f32, &str, Option<Value>)]) -> Value {
        json!({
            "items": rows.iter().map(|(_, _, v, _)| json!({"value": v})).collect::<Vec<_>>(),
            "hits": rows.iter().map(|(id, score, _, meta)| json!({
                "record_id": id, "source": "s", "score": score, "meta": meta})).collect::<Vec<_>>(),
        })
    }

    fn stored_meta(raw_offset: usize, doc_id: &str) -> Value {
        json!({
            "doc_id": doc_id, "raw_offset": raw_offset, "title": "A Title",
            "authors": ["A. Author"], "year": 2021, "doi": "10.1/x",
            "pdf_path": "pdf/xx/doc.pdf", "line_start": 3, "line_end": 9,
            "page": 4, "category": null,
        })
    }

    #[tokio::test]
    async fn hits_are_sorted_by_score_and_carry_the_raw_text_without_prefix_or_header() {
        let header = "A Title > chunk 0\n\n";
        let fake = stand_in(move |_, _| {
            // myelin's bookend order: strongest evidence at the edges.
            recall_reply(&[
                (
                    "b",
                    0.7,
                    &format!("document: {header}middle text"),
                    Some(stored_meta(header.len(), "d-b")),
                ),
                (
                    "a",
                    0.9,
                    &format!("document: {header}best text"),
                    Some(stored_meta(header.len(), "d-a")),
                ),
                (
                    "c",
                    0.7,
                    &format!("document: {header}tied text"),
                    Some(stored_meta(header.len(), "d-c")),
                ),
                ("z", 0.5, "document: x", None),
            ])
        })
        .await;
        let hits = store(&fake)
            .search("academic_papers", "a query", 10, &SearchFilter::default())
            .await
            .unwrap();
        let order: Vec<(&str, &str)> = hits
            .iter()
            .map(|h| (h.doc_id.as_str(), h.chunk_text.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("d-a", "best text"),
                ("d-b", "middle text"),
                ("d-c", "tied text")
            ],
            "ties break by record id; a record with no metadata is not a chunk"
        );
        let best = &hits[0];
        assert_eq!(best.title.as_deref(), Some("A Title"));
        assert_eq!(best.year, Some(2021));
        assert_eq!(best.page, Some(4));
        assert_eq!((best.line_start, best.line_end), (3, 9));
        assert!((best.score - 0.9).abs() < 1e-6);

        let (tool, args) = &tool_calls(&fake)[0];
        assert_eq!(tool, "recall");
        assert_eq!(args["query"], "a query");
        assert_eq!(args["tenant"], "academic_papers");
        assert_eq!(args["k"], 10);
        assert_eq!(args["budget_tokens"], 10 * 2048);
        assert_eq!(args["dated"], false);
        assert_eq!(args["filter"], json!([]));
    }

    #[tokio::test]
    async fn at_most_limit_hits_come_back() {
        let fake = stand_in(|_, _| {
            recall_reply(&[
                ("a", 0.9, "document: one", Some(stored_meta(0, "d"))),
                ("b", 0.8, "document: two", Some(stored_meta(0, "d"))),
                ("c", 0.7, "document: three", Some(stored_meta(0, "d"))),
            ])
        })
        .await;
        let hits = store(&fake)
            .search("c", "q", 2, &SearchFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].chunk_text, "two");
    }

    #[tokio::test]
    async fn search_filters_become_myelin_filter_conditions() {
        let fake = stand_in(|_, _| recall_reply(&[])).await;
        let s = store(&fake);
        let filter = |y: &str| SearchFilter {
            year: Some(YearFilter::parse(y).unwrap()),
            ..Default::default()
        };
        for y in [">=2021", ">2020", "<2019", "<=2018", "2023"] {
            s.search("c", "q", 5, &filter(y)).await.unwrap();
        }
        s.search(
            "c",
            "q",
            5,
            &SearchFilter {
                year: None,
                topic: Some("autism".into()),
                category: Some("medical".into()),
            },
        )
        .await
        .unwrap();
        let filters: Vec<Value> = tool_calls(&fake)
            .into_iter()
            .map(|(_, args)| args["filter"].clone())
            .collect();
        assert_eq!(
            filters[0],
            json!([{"op": "range", "key": "year", "gte": 2021}])
        );
        assert_eq!(
            filters[1],
            json!([{"op": "range", "key": "year", "gt": 2020}])
        );
        assert_eq!(
            filters[2],
            json!([{"op": "range", "key": "year", "lt": 2019}])
        );
        assert_eq!(
            filters[3],
            json!([{"op": "range", "key": "year", "lte": 2018}])
        );
        assert_eq!(
            filters[4],
            json!([{"op": "range", "key": "year", "gte": 2023, "lte": 2023}])
        );
        assert_eq!(
            filters[5],
            json!([
                {"op": "eq", "key": "topics", "value": "autism"},
                {"op": "eq", "key": "category", "value": "medical"},
            ])
        );
    }

    #[tokio::test]
    async fn a_recalled_value_without_the_document_prefix_is_an_error() {
        let fake = stand_in(|_, _| {
            recall_reply(&[("a", 0.9, "someone: text", Some(stored_meta(0, "d")))])
        })
        .await;
        let err = store(&fake)
            .search("c", "q", 5, &SearchFilter::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DistillError::Backend(m) if m.contains("document: ")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn items_and_hits_that_are_not_aligned_are_an_error() {
        let fake = stand_in(|_, _| json!({"items": [{"value": "document: x"}], "hits": []})).await;
        let err = store(&fake)
            .search("c", "q", 5, &SearchFilter::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    #[tokio::test]
    async fn a_header_offset_past_the_text_is_an_error_not_a_panic() {
        let fake = stand_in(|_, _| {
            recall_reply(&[("a", 0.9, "document: short", Some(stored_meta(500, "d")))])
        })
        .await;
        let err = store(&fake)
            .search("c", "q", 5, &SearchFilter::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    // ── forget / inventory / health ────────────────────────────────

    #[tokio::test]
    async fn forgetting_a_document_keeps_the_named_records() {
        let fake = stand_in(|_, _| json!({"affected": 3, "ids": ["x", "y", "z"]})).await;
        let affected = store(&fake)
            .forget_document("academic_papers", "doc", &["keep-1".to_string()])
            .await
            .unwrap();
        assert_eq!(affected, 3);
        let (tool, args) = &tool_calls(&fake)[0];
        assert_eq!(tool, "forget_scope");
        assert_eq!(
            *args,
            json!({
                "tenant": "academic_papers", "namespace": "doc",
                "keep": ["keep-1"], "confirm": true, "actor": "hs-distill",
            })
        );
    }

    #[tokio::test]
    async fn inventory_maps_namespaces_to_record_counts() {
        let fake = stand_in(|_, args| {
            let all = json!({"namespaces": [
                {"namespace": "a", "records": 3}, {"namespace": "b", "records": 1}],
                "total_records": 4});
            if args.get("namespace").is_some() {
                json!({"namespaces": [{"namespace": "a", "records": 3}], "total_records": 3})
            } else {
                all
            }
        })
        .await;
        let s = store(&fake);
        let all = s.inventory("academic_papers", None).await.unwrap();
        assert_eq!(all, BTreeMap::from([("a".into(), 3), ("b".into(), 1)]));
        let one = s.inventory("academic_papers", Some("a")).await.unwrap();
        assert_eq!(one, BTreeMap::from([("a".into(), 3)]));
        let calls = tool_calls(&fake);
        assert!(calls[0].1.get("namespace").is_none());
        assert_eq!(calls[1].1["namespace"], "a");
    }

    #[tokio::test]
    async fn health_reports_the_server_name_and_version() {
        let fake = stand_in(|_, _| json!({})).await;
        assert_eq!(store(&fake).health().await.unwrap(), "myelin-mcp 0.4.2");
        assert!(tool_calls(&fake).is_empty(), "health calls no tool");
    }

    #[tokio::test]
    async fn health_fails_when_myelin_is_unreachable() {
        let closed = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}/mcp", l.local_addr().unwrap())
        };
        let err = MyelinStore::new(&closed)
            .unwrap()
            .health()
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    // ── Against a real myelin ──────────────────────────────────────

    /// Round trip against a live `myelin-mcp` in a scratch tenant:
    /// `HS_DISTILL_LIVE_MYELIN_URL=http://127.0.0.1:7462/mcp cargo test -p
    /// hs-distill --features server -- --ignored live_myelin`. The scratch
    /// tenant is emptied afterwards; the home-still collections are never
    /// touched.
    #[tokio::test]
    #[ignore = "needs a running myelin-mcp (HS_DISTILL_LIVE_MYELIN_URL)"]
    async fn live_myelin_round_trip() {
        const TENANT: &str = "hs_distill_live_test";
        let url = std::env::var("HS_DISTILL_LIVE_MYELIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7462/mcp".into());
        let store = MyelinStore::new(&url).unwrap();
        assert!(store.health().await.unwrap().starts_with("myelin-mcp "));

        let mut first = chunk(
            "live-doc",
            0,
            2,
            "Hashimoto thyroiditis is an autoimmune disease.",
        );
        first.meta.publication_date = Some("2019-05-01".into());
        let mut second = chunk("live-doc", 1, 2, "Gazzaniga studied the split brain.");
        second.meta.publication_date = Some("2024-05-01".into());
        let result = async {
            let ids = store
                .write_document(TENANT, "live-doc", &[first.clone(), second.clone()])
                .await?;
            assert_eq!(ids.len(), 2);
            // Identical content: the same records, nothing new, nothing to forget.
            let again = store
                .write_document(TENANT, "live-doc", &[first.clone(), second.clone()])
                .await?;
            assert_eq!(again, ids);
            assert_eq!(store.forget_document(TENANT, "live-doc", &ids).await?, 0);
            assert_eq!(
                store.inventory(TENANT, None).await?,
                BTreeMap::from([("live-doc".to_string(), 2)])
            );

            let filter = SearchFilter {
                year: Some(YearFilter::Gte(2021)),
                ..Default::default()
            };
            let hits = store.search(TENANT, "split brain", 5, &filter).await?;
            assert_eq!(hits.len(), 1, "{hits:?}");
            assert_eq!(hits[0].chunk_text, "Gazzaniga studied the split brain.");
            assert_eq!(hits[0].year, Some(2024));
            assert_eq!(hits[0].title.as_deref(), Some("A Title"));
            assert_eq!(hits[0].page, Some(2));

            let all = store
                .search(TENANT, "Hashimoto", 5, &SearchFilter::default())
                .await?;
            assert!(all[0].chunk_text.contains("Hashimoto"), "{all:?}");
            assert!(all.windows(2).all(|w| w[0].score >= w[1].score));
            Ok::<_, DistillError>(())
        }
        .await;
        let removed = store.forget_document(TENANT, "live-doc", &[]).await;
        result.unwrap();
        assert_eq!(removed.unwrap(), 2);
        assert!(store.inventory(TENANT, None).await.unwrap().is_empty());
    }

    // ── sanitize_year ──────────────────────────────────────────────

    #[test]
    fn sanitize_year_keeps_plausible_publication_year() {
        assert_eq!(sanitize_year(Some("2018-05-15")), Some(2018));
        assert_eq!(sanitize_year(Some("1995")), Some(1995));
    }

    #[test]
    fn sanitize_year_keeps_next_year_preprint() {
        let next = chrono::Utc::now().year() as i64 + 1;
        let s = format!("{next}-01-01");
        assert_eq!(sanitize_year(Some(&s)), Some(next));
    }

    #[test]
    fn sanitize_year_rejects_far_future_year() {
        // The 2041 case from the self-test: regex-extracted citation year
        // bleeding into the metadata.
        assert_eq!(sanitize_year(Some("2041-01-01")), None);
    }

    #[test]
    fn sanitize_year_rejects_pre_1900() {
        assert_eq!(sanitize_year(Some("1899-12-31")), None);
        assert_eq!(sanitize_year(Some("0001")), None);
    }

    #[test]
    fn sanitize_year_handles_missing_or_unparseable() {
        assert_eq!(sanitize_year(None), None);
        assert_eq!(sanitize_year(Some("")), None);
        assert_eq!(sanitize_year(Some("XX")), None);
        assert_eq!(sanitize_year(Some("abcd-01-01")), None);
    }
}
