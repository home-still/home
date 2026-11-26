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
    /// Like `Json`, plus one extra response header.
    JsonWithHeader(u16, String, &'static str, &'static str),
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
                let r = reply(&req);
                let extra = match &r {
                    Reply::JsonWithHeader(_, _, k, v) => format!("{k}: {v}\r\n"),
                    _ => String::new(),
                };
                match r {
                    Reply::Json(status, body) | Reply::JsonWithHeader(status, body, ..) => {
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}\
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

// ── In-memory document store and ollama stand-in ───────────────────────

use std::collections::BTreeMap;

use async_trait::async_trait;
use hs_common::stores::{StoreSpec, Stores, PERSONAL_PREFIX_DEFAULT};

use crate::client::SearchHit;
use crate::config::DistillServerConfig;
use crate::error::DistillError;
use crate::store::{DocumentStore, SearchFilter};
use crate::types::{Chunk, ChunkSpan, DocumentMeta};

/// What the fake store was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Write {
        store: String,
        doc_id: String,
        /// Chunk indexes, in the order written.
        indexes: Vec<u32>,
    },
    Forget {
        store: String,
        doc_id: String,
        keep: Vec<String>,
    },
}

#[derive(Default)]
pub(crate) struct StoreState {
    /// (store, doc_id) -> every stored record, whatever its generation.
    pub docs: BTreeMap<(String, String), Vec<(String, Chunk)>>,
    pub ops: Vec<Op>,
    /// Every store that was defined, in order.
    pub defined: Vec<StoreSpec>,
    /// Refuse every `define_store` call.
    pub fail_define: bool,
    /// (stores, query, limit, filter) of every search.
    pub searches: Vec<(Vec<String>, String, u64, SearchFilter)>,
    /// Fail the n-th (0-based) `write_document` call and every later one.
    pub fail_write_from_call: Option<usize>,
    pub write_calls: usize,
    pub fail_forget: bool,
    /// Panic inside `write_document` (to model a bug in the write path).
    pub panic_on_write: bool,
    /// Give other tasks a turn once a write is stored, as a real store does
    /// while it waits for its answer.
    pub yield_after_write: bool,
    pub down: bool,
    /// What `search` returns, in ranking order.
    pub canned_hits: Vec<SearchHit>,
    next_record: u64,
    /// Every chunk ever written, in order.
    pub written: Vec<Chunk>,
}

/// In-memory [`DocumentStore`]: tracks which records each document has, so
/// tests can assert what is actually stored after a sequence of calls.
#[derive(Default)]
pub(crate) struct FakeStore {
    pub state: Mutex<StoreState>,
}

/// A chunk with no header, for seeding records.
pub(crate) fn blank_chunk(doc_id: &str, index: u32) -> Chunk {
    Chunk {
        doc_id: doc_id.into(),
        chunk_index: index,
        total_chunks: 0,
        text: String::new(),
        raw_text: String::new(),
        span: ChunkSpan {
            line_start: 0,
            line_end: 0,
            char_start: 0,
            char_end: 0,
        },
        page: None,
        meta: DocumentMeta::default(),
    }
}

impl StoreState {
    fn new_record(&mut self) -> String {
        self.next_record += 1;
        format!("record-{}", self.next_record)
    }
}

impl FakeStore {
    /// Chunk indexes of the records stored for `doc_id`, in storage order.
    pub fn stored(&self, store: &str, doc_id: &str) -> Vec<u32> {
        self.state
            .lock()
            .docs
            .get(&(store.to_string(), doc_id.to_string()))
            .map(|records| records.iter().map(|(_, c)| c.chunk_index).collect())
            .unwrap_or_default()
    }

    pub fn ops(&self) -> Vec<Op> {
        self.state.lock().ops.clone()
    }

    pub fn seed(&self, store: &str, doc_id: &str, chunks: u32) {
        let mut st = self.state.lock();
        let records = (0..chunks)
            .map(|i| (st.new_record(), blank_chunk(doc_id, i)))
            .collect();
        st.docs.insert((store.into(), doc_id.into()), records);
    }
}

#[async_trait]
impl DocumentStore for FakeStore {
    async fn health(&self) -> Result<String, DistillError> {
        if self.state.lock().down {
            return Err(DistillError::Backend("myelin unreachable: refused".into()));
        }
        Ok("fake-myelin 1.0.0".into())
    }

    async fn define_store(&self, spec: &StoreSpec) -> Result<(), DistillError> {
        let mut st = self.state.lock();
        if st.fail_define {
            return Err(DistillError::Backend("define refused".into()));
        }
        st.defined.push(spec.clone());
        Ok(())
    }

    async fn write_document(
        &self,
        store: &str,
        doc_id: &str,
        chunks: &[Chunk],
    ) -> Result<Vec<String>, DistillError> {
        let (ids, yield_now) = {
            let mut st = self.state.lock();
            if st.panic_on_write {
                panic!("simulated store bug");
            }
            let call = st.write_calls;
            st.write_calls += 1;
            if st.fail_write_from_call.is_some_and(|n| call >= n) {
                return Err(DistillError::Backend("write failed".into()));
            }
            let mut ids = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                let id = st.new_record();
                ids.push(id.clone());
                st.docs
                    .entry((store.to_string(), doc_id.to_string()))
                    .or_default()
                    .push((id, chunk.clone()));
            }
            st.written.extend(chunks.iter().cloned());
            st.ops.push(Op::Write {
                store: store.into(),
                doc_id: doc_id.into(),
                indexes: chunks.iter().map(|c| c.chunk_index).collect(),
            });
            (ids, st.yield_after_write)
        };
        if yield_now {
            tokio::task::yield_now().await;
        }
        Ok(ids)
    }

    async fn forget_document(
        &self,
        store: &str,
        doc_id: &str,
        keep: &[String],
    ) -> Result<u64, DistillError> {
        let mut st = self.state.lock();
        if st.fail_forget {
            return Err(DistillError::Backend("forget failed".into()));
        }
        st.ops.push(Op::Forget {
            store: store.into(),
            doc_id: doc_id.into(),
            keep: keep.to_vec(),
        });
        let key = (store.to_string(), doc_id.to_string());
        let Some(records) = st.docs.get_mut(&key) else {
            return Ok(0);
        };
        let before = records.len();
        records.retain(|(id, _)| keep.contains(id));
        let removed = (before - records.len()) as u64;
        if records.is_empty() {
            st.docs.remove(&key);
        }
        Ok(removed)
    }

    async fn search(
        &self,
        stores: &[&str],
        query: &str,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError> {
        let mut st = self.state.lock();
        st.searches.push((
            stores.iter().map(|s| s.to_string()).collect(),
            query.into(),
            limit,
            filter.clone(),
        ));
        Ok(st
            .canned_hits
            .iter()
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn inventory(
        &self,
        store: &str,
        doc_id: Option<&str>,
    ) -> Result<BTreeMap<String, u64>, DistillError> {
        let st = self.state.lock();
        Ok(st
            .docs
            .iter()
            .filter(|((s, d), records)| {
                s == store && doc_id.is_none_or(|want| want == d) && !records.is_empty()
            })
            .map(|((_, d), records)| (d.clone(), records.len() as u64))
            .collect())
    }
}

/// Where the ollama stand-in says the embedding model is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OllamaMode {
    Cuda,
    Cpu,
    Partial,
    /// Every request is a 500.
    Down,
}

/// A loopback ollama: `/api/ps` reports the embedding model as resident
/// where [`OllamaMode`] says (so no probe load is ever needed).
pub(crate) struct FakeOllama {
    pub http: FakeHttp,
    pub mode: Arc<Mutex<OllamaMode>>,
}

pub(crate) async fn fake_ollama(initial: OllamaMode) -> FakeOllama {
    let mode = Arc::new(Mutex::new(initial));
    let seen = mode.clone();
    let http = serve(move |_| {
        let (size, size_vram) = match *seen.lock() {
            OllamaMode::Cuda => (1_200_u64, 1_200_u64),
            OllamaMode::Cpu => (1_200, 0),
            OllamaMode::Partial => (1_200, 600),
            OllamaMode::Down => return Reply::Json(500, "ollama is down".into()),
        };
        Reply::Json(
            200,
            serde_json::json!({"models": [{
                "name": crate::embed_device::OLLAMA_EMBED_MODEL,
                "size": size, "size_vram": size_vram}]})
            .to_string(),
        )
    })
    .await;
    FakeOllama { http, mode }
}

/// A server state over fakes: the in-memory store, an ollama stand-in with
/// the model on the GPU, and the small-chunk config.
pub(crate) async fn fake_state() -> (crate::api::DistillServerState, Arc<FakeStore>, FakeOllama) {
    let ollama = fake_ollama(OllamaMode::Cuda).await;
    let store = Arc::new(FakeStore::default());
    let config = DistillServerConfig {
        ollama_url: ollama.http.url(),
        ..small_chunk_config()
    };
    let stores = Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT)
        .expect("the default personal prefix is a valid one");
    let state =
        crate::api::DistillServerState::new(store.clone(), reqwest::Client::new(), config, stores);
    (state, store, ollama)
}

/// A config small enough to produce several chunks from a few sentences:
/// ~240-char chunks with a ~40-char overlap.
pub(crate) fn small_chunk_config() -> DistillServerConfig {
    DistillServerConfig {
        chunk_max_tokens: 60,
        chunk_overlap: 10,
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

/// A page that is a reference list: every line is a citation. Short enough
/// to be one chunk under [`small_chunk_config`].
pub(crate) const BIBLIOGRAPHY: &str =
    "Berkenbosch F, van Vant J, Aubert A, et al. (2001) Neurotrophins and the brain. \
     Nature Reviews Neuroscience.\n\
     van Vant J, Jonkman T, Caligor L (2004) Neurobiology of learning and memory. \
     Journal of Neurochemistry.";
