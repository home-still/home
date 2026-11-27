//! Fakes for tool-level tests: a storage that fails on demand, a loopback
//! distill server, an event bus that remembers what was published, and a
//! synthetic OpenAlex corpus.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hs_common::event_bus::NoOpBus;
use hs_common::storage::{LocalFsStorage, ObjectMeta, Storage};

use crate::openalex::OpenAlexCorpus;
use crate::{Deps, HomeStillMcp};

/// A `LocalFsStorage` in a temp dir whose operations can be made to fail like
/// an S3 outage (an error that is *not* "not found").
pub struct FaultyStorage {
    inner: LocalFsStorage,
    _dir: tempfile::TempDir,
    pub fail_head: AtomicBool,
    pub fail_get: AtomicBool,
    pub fail_list: AtomicBool,
}

impl FaultyStorage {
    pub fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(Self {
            inner: LocalFsStorage::new(dir.path()),
            _dir: dir,
            fail_head: AtomicBool::new(false),
            fail_get: AtomicBool::new(false),
            fail_list: AtomicBool::new(false),
        })
    }

    pub fn set(flag: &AtomicBool, on: bool) {
        flag.store(on, Ordering::SeqCst);
    }

    fn outage(&self, flag: &AtomicBool, op: &str, key: &str) -> anyhow::Result<()> {
        if flag.load(Ordering::SeqCst) {
            anyhow::bail!("simulated storage outage during {op}({key})");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Storage for FaultyStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        self.outage(&self.fail_get, "get", key)?;
        self.inner.get(key).await
    }
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
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
        self.inner.delete(key).await
    }
}

/// How many records [`FakeDistill`] says each store holds for a document it
/// knows.
pub const FAKE_RECORDS_PER_DOC: u64 = 4;

/// A loopback distill server that knows the documents it was started with.
/// It answers the two routes the `status` tool asks, `GET /status` and
/// `GET /exists/<doc_id>`, for whichever store the request names.
pub struct FakeDistill {
    pub url: String,
}

impl FakeDistill {
    /// `doc_ids` are the documents every store is said to hold.
    pub async fn start(doc_ids: Vec<String>) -> Self {
        use axum::extract::Request;
        use axum::response::Json;

        let app = axum::Router::new().fallback(move |req: Request| {
            let doc_ids = doc_ids.clone();
            async move {
                let store = req
                    .uri()
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("store="))
                    .unwrap_or_default()
                    .to_string();
                let path = req.uri().path();
                if path == "/status" {
                    return Json(serde_json::json!({
                        "store": store,
                        "records_count": doc_ids.len() as u64 * FAKE_RECORDS_PER_DOC,
                        "documents_count": doc_ids.len(),
                        "documents_count_truncated": false,
                        "compute_device": "cuda",
                        "embed_model": "bge-m3",
                    }));
                }
                match path.strip_prefix("/exists/") {
                    Some(doc_id) => {
                        let known = doc_ids.iter().any(|id| id == doc_id);
                        Json(serde_json::json!({
                            "exists": known,
                            "chunks": if known { FAKE_RECORDS_PER_DOC } else { 0 },
                        }))
                    }
                    None => Json(serde_json::json!({})),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url }
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
        paper_config: paper::config::Config::default(),
        distill_servers,
        openalex: OpenAlexCorpus::Unavailable(
            "the test server has no OpenAlex corpus; build one with `hs openalex load-works`"
                .to_string(),
        ),
        // The tests do not configure links, so the tools carry none.
        link_minter: None,
        stores: hs_common::stores::Stores::with_personal_prefix(
            hs_common::stores::PERSONAL_PREFIX_DEFAULT,
        )
        .unwrap(),
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
/// on: enough for the tools that store a document and announce it, and for
/// proving that a source is refused before anything is announced.
pub fn server_with_bus(storage: Arc<dyn Storage>, bus: Arc<RecordingBus>) -> HomeStillMcp {
    HomeStillMcp::from_deps(deps(
        storage,
        bus,
        vec!["http://127.0.0.1:9".to_string()],
        Vec::new(),
    ))
    .unwrap()
}

/// [`server_with_bus`] with a link minter, so the `links`-bearing tools can be
/// exercised end to end.
///
/// The base URLs are fixed, the key is fixed, and the corpus is whatever
/// `storage` holds — which is what a short link's record is written into.
pub fn server_with_links(storage: Arc<dyn Storage>, bus: Arc<RecordingBus>) -> HomeStillMcp {
    linked(storage, bus, "http://host:7446")
}

/// [`server_with_links`] whose LAN links point at `lan_base`: a docs
/// service a test stands up.
pub fn server_with_docs_service(storage: Arc<dyn Storage>, lan_base: &str) -> HomeStillMcp {
    linked(storage, Arc::new(RecordingBus::default()), lan_base)
}

fn linked(storage: Arc<dyn Storage>, bus: Arc<RecordingBus>, lan_base: &str) -> HomeStillMcp {
    let mut d = deps(
        storage,
        bus,
        vec!["http://127.0.0.1:9".to_string()],
        Vec::new(),
    );
    d.link_minter = Some(crate::links::LinkMinter::for_test(
        hs_common::docs::LinkKey::from_bytes(vec![9u8; 32]).unwrap(),
        lan_base,
        "https://public.example",
    ));
    HomeStillMcp::from_deps(d).unwrap()
}

/// [`server`] over an arbitrary event bus (for example a lazily connected
/// one whose broker is down).
pub fn server_with_event_bus(
    storage: Arc<dyn Storage>,
    events: Arc<dyn hs_common::event_bus::EventBus>,
) -> HomeStillMcp {
    HomeStillMcp::from_deps(deps(storage, events, Vec::new(), Vec::new())).unwrap()
}

/// A server over a synthetic OpenAlex DuckDB (a file in `dir`, never the real
/// corpus) holding a handful of works: W1, W2 and W3 each cite W9. There is
/// no full-text index, so only the DOI lookups and the citation graph can be
/// asked of it.
pub fn server_with_openalex(dir: &std::path::Path) -> HomeStillMcp {
    let conn = duckdb::Connection::open(dir.join("openalex.duckdb")).unwrap();
    conn.execute_batch(
        "CREATE TABLE _corpus_state(component VARCHAR);
         INSERT INTO _corpus_state VALUES ('openalex_works');
         CREATE TABLE works(openalex_id VARCHAR, doi VARCHAR, title VARCHAR, abstract_text VARCHAR,
             publication_year INTEGER, publication_date VARCHAR, language VARCHAR, type VARCHAR,
             cited_by_count BIGINT, is_retracted BOOLEAN, is_oa BOOLEAN, oa_url VARCHAR,
             primary_source_id VARCHAR);
         INSERT INTO works VALUES
           ('W1', '10.1234/abc', 'Old paper', NULL, 1999, NULL, 'en', 'article', 5, false, false, NULL, NULL),
           ('W2', '10.1234/new', 'New paper', NULL, 2020, NULL, 'en', 'article', 9, false, false, NULL, NULL),
           ('W3', NULL, 'Undated paper', NULL, NULL, NULL, 'en', 'article', NULL, false, false, NULL, NULL),
           ('W9', '10.1234/cited', 'Cited work', NULL, 1990, NULL, 'en', 'article', 3, false, false, NULL, NULL);
         CREATE TABLE work_references(work_id VARCHAR, referenced_work_id VARCHAR);
         INSERT INTO work_references VALUES ('W1','W9'), ('W2','W9'), ('W3','W9');
         CREATE TABLE work_authorships(work_id VARCHAR, author_id VARCHAR, author_position VARCHAR,
             raw_affiliation_string VARCHAR, institution_id VARCHAR);
         CREATE TABLE authors(openalex_id VARCHAR, display_name VARCHAR, cited_by_count BIGINT);
         CREATE TABLE work_topics(work_id VARCHAR, topic_id VARCHAR, score DOUBLE);
         CREATE TABLE topics(openalex_id VARCHAR, display_name VARCHAR);",
    )
    .unwrap();
    let mut d = deps(
        FaultyStorage::new(),
        Arc::new(NoOpBus),
        Vec::new(),
        Vec::new(),
    );
    d.openalex = OpenAlexCorpus::Ready(Arc::new(Mutex::new(conn)));
    HomeStillMcp::from_deps(d).unwrap()
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
