//! Test-only loopback HTTP server. Binds an ephemeral 127.0.0.1 port, so
//! tests never reach a real service.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    pub request_line: String,
    pub body: String,
}

pub(crate) enum Reply {
    /// Respond with this status and JSON body.
    Json(u16, String),
    /// Accept the request and never answer.
    Hang,
}

pub(crate) struct FakeHttp {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeHttp {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().clone()
    }
}

/// Start a server that answers every request with `reply(&request)`.
pub(crate) async fn serve(reply: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> FakeHttp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(reply);
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.clone();
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                recorded.lock().push(req.clone());
                match reply(&req) {
                    Reply::Json(status, body) => {
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Hang => tokio::time::sleep(Duration::from_secs(3600)).await,
                }
            });
        }
    });
    FakeHttp { addr, requests }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        request_line: head.lines().next().unwrap_or_default().to_string(),
        body: String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string(),
    })
}

// ── In-memory embedder and vector store ────────────────────────────────

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;

use crate::client::SearchHit;
use crate::collection::CollectionSpec;
use crate::config::{ComputeDevice, DistillServerConfig, EmbeddingConfig};
use crate::embed::{Embedder, EmbedderHealth};
use crate::error::DistillError;
use crate::store::{DocIds, SearchFilter, VectorStore};
use crate::types::{EmbeddedChunk, EmbeddingOutput, ScrubReport};

pub(crate) const FAKE_DIM: usize = 4;

/// Embedder that returns a fixed-width vector per text and records what it
/// was asked to embed.
pub(crate) struct FakeEmbedder {
    pub texts: Mutex<Vec<Vec<String>>>,
    pub fail: Mutex<Option<String>>,
    pub health: Mutex<EmbedderHealth>,
    /// Return this many fewer vectors than texts (to model a short batch).
    pub short_by: Mutex<usize>,
    pub slots: usize,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            texts: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            health: Mutex::new(EmbedderHealth::Healthy),
            short_by: Mutex::new(0),
            slots: 2,
        }
    }

    pub fn calls(&self) -> usize {
        self.texts.lock().len()
    }

    pub fn embedded_text_count(&self) -> usize {
        self.texts.lock().iter().map(Vec::len).sum()
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<EmbeddingOutput>, DistillError> {
        if let Some(msg) = self.fail.lock().clone() {
            return Err(DistillError::Embedding(msg));
        }
        let n = texts.len().saturating_sub(*self.short_by.lock());
        let out = texts
            .iter()
            .take(n)
            .map(|t| EmbeddingOutput {
                dense: vec![t.len() as f32; FAKE_DIM],
            })
            .collect();
        self.texts.lock().push(texts);
        Ok(out)
    }

    fn dimension(&self) -> usize {
        FAKE_DIM
    }

    fn device(&self) -> &ComputeDevice {
        &ComputeDevice::Cuda
    }

    fn health(&self) -> EmbedderHealth {
        self.health.lock().clone()
    }

    fn slots(&self) -> usize {
        self.slots
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Upsert {
        collection: String,
        doc_id: String,
        indexes: Vec<u32>,
    },
    DeleteFrom {
        collection: String,
        doc_id: String,
        from: u32,
    },
    Reset {
        collection: String,
    },
    Scrub {
        collection: String,
        dry_run: bool,
    },
}

#[derive(Default)]
pub(crate) struct StoreState {
    /// (collection, doc_id) -> stored chunk indexes.
    pub docs: BTreeMap<(String, String), BTreeSet<u32>>,
    pub ops: Vec<Op>,
    pub searches: Vec<(String, u64, SearchFilter)>,
    /// Fail the n-th (0-based) `upsert` call and every later one.
    pub fail_upsert_from_call: Option<usize>,
    pub upsert_calls: usize,
    pub fail_delete: bool,
    pub down: bool,
    pub canned_hits: Vec<SearchHit>,
    /// Every chunk ever upserted, in order.
    pub upserted: Vec<EmbeddedChunk>,
}

/// In-memory [`VectorStore`]: tracks which chunk indexes each document has,
/// so tests can assert what is actually stored after a sequence of calls.
#[derive(Default)]
pub(crate) struct FakeStore {
    pub state: Mutex<StoreState>,
}

impl FakeStore {
    pub fn stored(&self, collection: &str, doc_id: &str) -> Vec<u32> {
        self.state
            .lock()
            .docs
            .get(&(collection.to_string(), doc_id.to_string()))
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn ops(&self) -> Vec<Op> {
        self.state.lock().ops.clone()
    }

    pub fn seed(&self, collection: &str, doc_id: &str, chunks: u32) {
        self.state
            .lock()
            .docs
            .insert((collection.into(), doc_id.into()), (0..chunks).collect());
    }
}

#[async_trait]
impl VectorStore for FakeStore {
    async fn health(&self) -> Result<String, DistillError> {
        if self.state.lock().down {
            return Err(DistillError::Qdrant("qdrant unreachable: refused".into()));
        }
        Ok("1.0.0".into())
    }

    async fn upsert(&self, collection: &str, chunks: &[EmbeddedChunk]) -> Result<(), DistillError> {
        let mut st = self.state.lock();
        let call = st.upsert_calls;
        st.upsert_calls += 1;
        if st.fail_upsert_from_call.is_some_and(|n| call >= n) {
            return Err(DistillError::Qdrant("upsert failed".into()));
        }
        let Some(first) = chunks.first() else {
            return Ok(());
        };
        let doc_id = first.chunk.doc_id.clone();
        st.upserted.extend(chunks.iter().cloned());
        let indexes: Vec<u32> = chunks.iter().map(|c| c.chunk.chunk_index).collect();
        st.docs
            .entry((collection.to_string(), doc_id.clone()))
            .or_default()
            .extend(indexes.iter().copied());
        st.ops.push(Op::Upsert {
            collection: collection.into(),
            doc_id,
            indexes,
        });
        Ok(())
    }

    async fn delete_chunks_from(
        &self,
        collection: &str,
        doc_id: &str,
        from_chunk: u32,
    ) -> Result<(), DistillError> {
        let mut st = self.state.lock();
        if st.fail_delete {
            return Err(DistillError::Qdrant("delete failed".into()));
        }
        if let Some(set) = st
            .docs
            .get_mut(&(collection.to_string(), doc_id.to_string()))
        {
            set.retain(|&i| i < from_chunk);
        }
        st.ops.push(Op::DeleteFrom {
            collection: collection.into(),
            doc_id: doc_id.into(),
            from: from_chunk,
        });
        Ok(())
    }

    async fn doc_chunks(&self, collection: &str, doc_id: &str) -> Result<u64, DistillError> {
        Ok(self.stored(collection, doc_id).len() as u64)
    }

    async fn search(
        &self,
        collection: &str,
        _vector: Vec<f32>,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError> {
        let mut st = self.state.lock();
        st.searches.push((collection.into(), limit, filter.clone()));
        Ok(st.canned_hits.clone())
    }

    async fn points_count(&self, collection: &str) -> Result<u64, DistillError> {
        Ok(self
            .state
            .lock()
            .docs
            .iter()
            .filter(|((c, _), _)| c == collection)
            .map(|(_, s)| s.len() as u64)
            .sum())
    }

    async fn doc_ids(&self, collection: &str, limit: u64) -> Result<DocIds, DistillError> {
        let st = self.state.lock();
        let mut ids: Vec<String> = st
            .docs
            .iter()
            .filter(|((c, _), s)| c == collection && !s.is_empty())
            .map(|((_, d), _)| d.clone())
            .collect();
        let truncated = ids.len() as u64 > limit;
        ids.truncate(limit as usize);
        Ok(DocIds { ids, truncated })
    }

    async fn reset(&self, collection: &str, _spec: &CollectionSpec) -> Result<u64, DistillError> {
        let mut st = self.state.lock();
        let prior: u64 = st
            .docs
            .iter()
            .filter(|((c, _), _)| c == collection)
            .map(|(_, s)| s.len() as u64)
            .sum();
        st.docs.retain(|(c, _), _| c != collection);
        st.ops.push(Op::Reset {
            collection: collection.into(),
        });
        Ok(prior)
    }

    async fn scrub_interstitials(
        &self,
        collection: &str,
        dry_run: bool,
    ) -> Result<ScrubReport, DistillError> {
        self.state.lock().ops.push(Op::Scrub {
            collection: collection.into(),
            dry_run,
        });
        Ok(ScrubReport {
            total_scanned: 0,
            matched: 0,
            deleted: 0,
            samples: Vec::new(),
        })
    }
}

/// A config small enough to produce several chunks from a few sentences:
/// ~240-char chunks with a ~40-char overlap, and an embedder window that
/// satisfies the chunk/window rule.
pub(crate) fn small_chunk_config() -> DistillServerConfig {
    DistillServerConfig {
        chunk_max_tokens: 60,
        chunk_overlap: 10,
        embedding: EmbeddingConfig {
            max_length: 128,
            ..EmbeddingConfig::default()
        },
        ..DistillServerConfig::default()
    }
}

/// Distinct sentences, so chunks pass the quality filter.
pub(crate) fn prose(sentences: usize) -> String {
    (0..sentences)
        .map(|i| {
            format!(
                "Sentence number {i} explains finding {i} about subject {} in detail.",
                i * 7 + 3
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
