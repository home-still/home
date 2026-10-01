//! Fakes for tool-level tests: a storage that fails on demand and records
//! deletes, and a loopback distill server that records every request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hs_common::event_bus::NoOpBus;
use hs_common::storage::{LocalFsStorage, ObjectMeta, Storage};

use crate::{Deps, HomeStillMcp};

/// A `LocalFsStorage` in a temp dir whose operations can be made to fail like
/// an S3 outage (an error that is *not* "not found"), and that records every
/// `delete`.
pub struct FaultyStorage {
    inner: LocalFsStorage,
    _dir: tempfile::TempDir,
    pub fail_head: AtomicBool,
    pub fail_get: AtomicBool,
    pub fail_put: AtomicBool,
    pub fail_list: AtomicBool,
    /// When set, faults only hit keys containing this text (so a test can
    /// break the markdown probe and leave catalog reads working).
    pub only_keys_containing: Mutex<Option<String>>,
    pub deletes: Mutex<Vec<String>>,
}

impl FaultyStorage {
    pub fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(Self {
            inner: LocalFsStorage::new(dir.path()),
            _dir: dir,
            fail_head: AtomicBool::new(false),
            fail_get: AtomicBool::new(false),
            fail_put: AtomicBool::new(false),
            fail_list: AtomicBool::new(false),
            only_keys_containing: Mutex::new(None),
            deletes: Mutex::new(Vec::new()),
        })
    }

    pub fn set(flag: &AtomicBool, on: bool) {
        flag.store(on, Ordering::SeqCst);
    }

    fn outage(&self, flag: &AtomicBool, op: &str, key: &str) -> anyhow::Result<()> {
        let in_scope = self
            .only_keys_containing
            .lock()
            .unwrap()
            .as_deref()
            .is_none_or(|needle| key.contains(needle));
        if in_scope && flag.load(Ordering::SeqCst) {
            anyhow::bail!("simulated storage outage during {op}({key})");
        }
        Ok(())
    }

    pub fn recorded_deletes(&self) -> Vec<String> {
        self.deletes.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Storage for FaultyStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        self.outage(&self.fail_get, "get", key)?;
        self.inner.get(key).await
    }
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.outage(&self.fail_put, "put", key)?;
        self.inner.put(key, bytes).await
    }
    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        self.outage(&self.fail_head, "head", key)?;
        self.inner.head(key).await
    }
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        self.outage(&self.fail_list, "list", prefix)?;
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.deletes.lock().unwrap().push(key.to_string());
        self.inner.delete(key).await
    }
}

/// One request seen by [`FakeDistill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: String,
}

/// A loopback distill server. Answers the routes the client uses with canned
/// replies and records every request, so a test can assert what was (not)
/// called.
pub struct FakeDistill {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeDistill {
    /// `doc_ids` is what `GET /docs` returns.
    pub async fn start(doc_ids: Vec<String>) -> Self {
        use axum::extract::Request;
        use axum::response::Json;

        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let app = axum::Router::new().fallback(move |req: Request| {
            let recorder = recorder.clone();
            let doc_ids = doc_ids.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let query = req.uri().query().unwrap_or_default().to_string();
                recorder.lock().unwrap().push(Seen {
                    method: method.clone(),
                    path: path.clone(),
                    query,
                });
                match (method.as_str(), path.as_str()) {
                    ("POST", "/distill") => Json(serde_json::json!({
                        "doc_id": "doc",
                        "chunks_indexed": 3,
                        "embedding_device": "cuda",
                    })),
                    ("GET", "/docs") => Json(serde_json::json!({
                        "doc_ids": doc_ids,
                        "truncated": false,
                    })),
                    ("DELETE", _) => Json(serde_json::json!({ "deleted": 7 })),
                    _ => Json(serde_json::json!({})),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, seen }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|s| s.method == method)
            .count()
    }
}

fn deps(
    storage: Arc<dyn Storage>,
    events: Arc<dyn hs_common::event_bus::EventBus>,
    scribe_servers: Vec<String>,
    distill_servers: Vec<String>,
) -> Deps {
    Deps {
        storage,
        events,
        scribe_servers,
        scribe_convert_timeout: std::time::Duration::from_secs(5),
        scribe_timeout_policy: hs_scribe::config::TimeoutPolicy::default(),
        epub_limits: hs_scribe::epub::EpubLimits::default(),
        paper_config: paper::config::Config::default(),
        distill_servers,
        openalex_db: None,
    }
}

/// A server over `storage` with the given distill backend and nothing else
/// configured.
pub fn server(storage: Arc<dyn Storage>, distill: Option<&FakeDistill>) -> HomeStillMcp {
    HomeStillMcp::from_deps(deps(
        storage,
        Arc::new(NoOpBus),
        Vec::new(),
        distill.map(|d| d.url.clone()).into_iter().collect(),
    ))
    .unwrap()
}

/// An event bus that remembers what was published.
#[derive(Default)]
pub struct RecordingBus {
    pub published: Mutex<Vec<(String, serde_json::Value)>>,
}

#[async_trait::async_trait]
impl hs_common::event_bus::EventBus for RecordingBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
        self.published
            .lock()
            .unwrap()
            .push((subject.to_string(), serde_json::from_slice(payload)?));
        Ok(())
    }

    async fn consume(
        &self,
        _spec: &hs_common::event_bus::ConsumerSpec,
    ) -> anyhow::Result<hs_common::event_bus::EventStream> {
        anyhow::bail!("RecordingBus only records publishes")
    }
}

/// [`server`] with a recording bus and a scribe server that nothing listens
/// on: enough for the sources that never reach the scribe (HTML, EPUB) and
/// for proving that a source is refused before any request is made.
pub fn server_with_bus(storage: Arc<dyn Storage>, bus: Arc<RecordingBus>) -> HomeStillMcp {
    HomeStillMcp::from_deps(deps(
        storage,
        bus,
        vec!["http://127.0.0.1:9".to_string()],
        Vec::new(),
    ))
    .unwrap()
}

/// Put a markdown document (and optionally a catalog row) into `storage`.
pub async fn seed_markdown(storage: &dyn Storage, stem: &str, body: &str) {
    storage
        .put(
            &hs_common::markdown::markdown_storage_key(stem),
            body.as_bytes().to_vec(),
        )
        .await
        .unwrap();
}

pub async fn seed_catalog(storage: &dyn Storage, stem: &str) {
    let entry = hs_common::catalog::CatalogEntry {
        title: Some("A title".into()),
        ..Default::default()
    };
    hs_common::catalog::write_catalog_entry_via(storage, "catalog", stem, &entry)
        .await
        .unwrap();
}
