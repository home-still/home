//! Behaviour of the download path against a loopback fake server.
//!
//! No test here reaches a real host: every fixed source (arXiv, MDPI,
//! Unpaywall, NCBI, PMC) is pointed at [`FakeServer`] through
//! `PaperDownloader::for_tests`, which is `cfg(test)`-only and admits loopback
//! (and nothing else private). Tests that prove the *production* policy build
//! the downloader with `with_event_bus` and assert the fake server saw no
//! request at all.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use hs_common::event_bus::NoOpBus;
use hs_common::storage::{LocalFsStorage, ObjectMeta};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::config::DownloadConfig;
use crate::error::OutcomeKind;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use crate::services::download::download_batch;

// ---------------------------------------------------------------------------
// Fake HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Route {
    Body {
        content_type: &'static str,
        body: Vec<u8>,
    },
    Status(u16),
    Redirect(String),
    /// Advertise `declared` bytes of `Content-Length`, send only `body`, close.
    LyingLength {
        declared: u64,
        body: Vec<u8>,
    },
    /// A chunked PDF that never ends, until the client goes away.
    Endless,
}

struct FakeServer {
    base: String,
    /// Request targets (path + query) in arrival order.
    hits: Arc<Mutex<Vec<String>>>,
    /// Body bytes the server managed to write.
    sent: Arc<AtomicU64>,
}

impl FakeServer {
    async fn start(router: impl Fn(&str) -> Route + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(AtomicU64::new(0));
        let router = Arc::new(router);
        let (h, s) = (hits.clone(), sent.clone());
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve_one(sock, router.clone(), h.clone(), s.clone()));
            }
        });
        Self {
            base: format!("http://{addr}"),
            hits,
            sent,
        }
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

async fn serve_one(
    mut sock: tokio::net::TcpStream,
    router: Arc<dyn Fn(&str) -> Route + Send + Sync>,
    hits: Arc<Mutex<Vec<String>>>,
    sent: Arc<AtomicU64>,
) {
    let mut head = Vec::new();
    let mut tmp = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&tmp[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    hits.lock().push(target.clone());

    match router(&target) {
        Route::Body { content_type, body } => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(header.as_bytes()).await;
            if sock.write_all(&body).await.is_ok() {
                sent.fetch_add(body.len() as u64, Ordering::Relaxed);
            }
        }
        Route::Status(code) => {
            let header =
                format!("HTTP/1.1 {code} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(header.as_bytes()).await;
        }
        Route::Redirect(location) => {
            let header = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(header.as_bytes()).await;
        }
        Route::LyingLength { declared, body } => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(header.as_bytes()).await;
            if sock.write_all(&body).await.is_ok() {
                sent.fetch_add(body.len() as u64, Ordering::Relaxed);
            }
        }
        Route::Endless => {
            let header = "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            if sock.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let mut chunk = vec![b' '; 16 * 1024];
            chunk[..9].copy_from_slice(b"%PDF-1.4\n");
            let framed = {
                let mut f = format!("{:x}\r\n", chunk.len()).into_bytes();
                f.extend_from_slice(&chunk);
                f.extend_from_slice(b"\r\n");
                f
            };
            // 512 MiB is a safety valve: a downloader that never aborts fails
            // the test on the `sent` assertion instead of hanging it.
            while sent.load(Ordering::Relaxed) < 512 * 1024 * 1024 {
                if sock.write_all(&framed).await.is_err() {
                    return;
                }
                sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const CONTACT: &str = "ops@example.org";

fn pdf(total: usize) -> Vec<u8> {
    let mut v = b"%PDF-1.4\n".to_vec();
    v.resize(total.max(v.len()), b' ');
    v
}

fn pdf_route(total: usize) -> Route {
    Route::Body {
        content_type: "application/pdf",
        body: pdf(total),
    }
}

fn html_route() -> Route {
    let mut body =
        b"<!DOCTYPE html><html><head><title>Repository record</title></head><body>".to_vec();
    body.resize(4000, b' ');
    Route::Body {
        content_type: "text/html",
        body,
    }
}

fn config(max_bytes: u64) -> DownloadConfig {
    DownloadConfig {
        max_download_bytes: max_bytes,
        unpaywall_email: Some(CONTACT.to_string()),
        timeout_secs: 20,
        ..Default::default()
    }
}

/// Delegates to a real `LocalFsStorage`, optionally failing `put`/`head`.
struct FlakyStorage {
    inner: LocalFsStorage,
    fail_put: bool,
    fail_head: bool,
}

#[async_trait]
impl Storage for FlakyStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.get(key).await
    }
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        if self.fail_put {
            anyhow::bail!("disk full");
        }
        self.inner.put(key, bytes).await
    }
    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        if self.fail_head {
            anyhow::bail!("connection reset by peer");
        }
        self.inner.head(key).await
    }
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key).await
    }
}

struct Rig {
    dl: PaperDownloader,
    storage: Arc<LocalFsStorage>,
    _tmp: tempfile::TempDir,
}

impl Rig {
    fn new(
        server: &FakeServer,
        cfg: &DownloadConfig,
        resolvers: Vec<Arc<dyn PaperProvider>>,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalFsStorage::new(tmp.path()));
        let dl = PaperDownloader::for_tests(
            storage.clone(),
            Arc::new(NoOpBus),
            cfg,
            resolvers,
            &server.base,
        );
        Self {
            dl,
            storage,
            _tmp: tmp,
        }
    }

    /// A downloader built the way production builds it (loopback refused).
    fn production(cfg: &DownloadConfig) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalFsStorage::new(tmp.path()));
        let dl = PaperDownloader::with_event_bus(storage.clone(), Arc::new(NoOpBus), cfg, vec![])
            .unwrap();
        Self {
            dl,
            storage,
            _tmp: tmp,
        }
    }

    async fn stored(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .storage
            .list("")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect();
        keys.sort();
        keys
    }
}

struct FakeProvider {
    name: &'static str,
    reply: Box<dyn Fn() -> Result<Option<Paper>, PaperError> + Send + Sync>,
}

#[async_trait]
impl PaperProvider for FakeProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![]
    }
    async fn search_by_query(&self, _q: &SearchQuery) -> Result<SearchResult, PaperError> {
        Err(PaperError::ProviderUnavailable("not used".into()))
    }
    async fn get_by_doi(&self, _doi: &str) -> Result<Option<Paper>, PaperError> {
        (self.reply)()
    }
}

fn paper_with(id: &str, doi: Option<&str>, urls: Vec<String>) -> Paper {
    Paper {
        id: id.to_string(),
        title: format!("paper {id}"),
        authors: vec![],
        abstract_text: None,
        publication_date: None,
        doi: doi.map(str::to_string),
        download_urls: urls,
        cited_by_count: None,
        source: "test".to_string(),
    }
}

fn provider_returning(name: &'static str, urls: Vec<String>) -> Arc<dyn PaperProvider> {
    Arc::new(FakeProvider {
        name,
        reply: Box::new(move || Ok(Some(paper_with("p", Some("10.1234/x"), urls.clone())))),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// RA-11: bounded, validated, PDF-only downloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_within_the_cap_is_stored_hashed_and_verified() {
    let body = pdf(50_000);
    let b = body.clone();
    let server = FakeServer::start(move |_| Route::Body {
        content_type: "application/octet-stream", // the magic decides, not the label
        body: b.clone(),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let cb = move |done: u64, total: Option<u64>| seen2.lock().push((done, total));
    let result = rig
        .dl
        .download_by_url(&server.url("/paper.pdf"), "10.1234_ok", Some(&cb))
        .await
        .unwrap();

    assert!(!result.skipped);
    assert_eq!(result.size_bytes, body.len() as u64);
    assert_eq!(result.sha256, sha256_hex(&body));
    assert_eq!(result.file_path, PathBuf::from("papers/10/10.1234_ok.pdf"));
    assert_eq!(
        rig.storage.get("papers/10/10.1234_ok.pdf").await.unwrap(),
        body
    );
    let seen = seen.lock();
    assert_eq!(seen.last().map(|p| p.0), Some(body.len() as u64));
    assert_eq!(seen.last().and_then(|p| p.1), Some(body.len() as u64));
}

#[tokio::test]
async fn an_endless_body_is_aborted_at_the_cap_and_nothing_is_stored() {
    let server = FakeServer::start(|_| Route::Endless).await;
    let cap = 1024 * 1024;
    let rig = Rig::new(&server, &config(cap), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/huge.pdf"), "10.1234_huge", None)
        .await
        .unwrap_err();

    assert!(
        matches!(err, PaperError::TooLarge { limit, .. } if limit == cap),
        "{err}"
    );
    assert!(
        rig.stored().await.is_empty(),
        "no partial object may be stored"
    );
    // The client hung up at ~1 MiB; the server was never allowed to push the
    // 512 MiB safety valve. (Socket buffers account for the slack.)
    let sent = server.sent.load(Ordering::Relaxed);
    assert!(sent < 64 * 1024 * 1024, "server wrote {sent} bytes");
}

#[tokio::test]
async fn a_lying_content_length_is_refused_without_trusting_it_for_an_allocation() {
    // 16 EiB declared: `Vec::with_capacity(content_length)` aborted the
    // process on this before the cap existed.
    let server = FakeServer::start(|_| Route::LyingLength {
        declared: u64::MAX / 2,
        body: pdf(200),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/lie.pdf"), "10.1234_lie", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::TooLarge { .. }), "{err}");
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn a_declared_length_just_under_the_cap_does_not_reserve_the_cap() {
    // Declares the full cap but sends 200 bytes then closes: the transfer
    // fails on the truncated body, and (by construction) only
    // `PREALLOC_CAP` was ever reserved up front.
    let server = FakeServer::start(|_| Route::LyingLength {
        declared: 200 * 1024 * 1024,
        body: pdf(200),
    })
    .await;
    let rig = Rig::new(&server, &config(256 * 1024 * 1024), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/short.pdf"), "10.1234_short", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::Http(_)), "{err}");
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn an_html_body_is_rejected_not_stored_as_a_paper() {
    let server = FakeServer::start(|_| html_route()).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/landing"), "10.1234_landing", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::NotPdf { .. }), "{err}");
    assert!(err.to_string().contains("HTML"), "{err}");
    assert!(
        rig.stored().await.is_empty(),
        "an HTML landing page must leave no object (BACKLOG P1-14)"
    );
}

#[tokio::test]
async fn an_html_body_labelled_application_pdf_is_still_rejected() {
    let server = FakeServer::start(|_| Route::Body {
        content_type: "application/pdf",
        body: b"<html><body>Please sign in to download</body></html>".repeat(10),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/x.pdf"), "10.1234_label", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::NotPdf { .. }), "{err}");
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn non_pdf_binaries_and_pdf_stubs_are_rejected() {
    let jpeg = {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0];
        b.resize(5000, 0);
        b
    };
    for (name, body) in [
        ("jpeg", jpeg),
        ("empty", Vec::new()),
        ("magic-only-stub", b"%PDF-1.4\n".to_vec()),
        ("four-bytes", b"%PDF".to_vec()),
    ] {
        let b = body.clone();
        let server = FakeServer::start(move |_| Route::Body {
            content_type: "application/pdf",
            body: b.clone(),
        })
        .await;
        let rig = Rig::new(&server, &config(1_000_000), vec![]);
        let err = rig
            .dl
            .download_by_url(&server.url("/x"), "10.1234_bin", None)
            .await
            .unwrap_err();
        assert!(matches!(err, PaperError::NotPdf { .. }), "{name}: {err}");
        assert!(rig.stored().await.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_non_pdf_is_abandoned_after_its_first_chunk() {
    // A big HTML body must not be downloaded to completion just to be thrown
    // away: the gate runs on the first bytes.
    let server = FakeServer::start(|_| Route::Body {
        content_type: "text/html",
        body: {
            let mut b = b"<!doctype html><html>".to_vec();
            b.resize(8 * 1024 * 1024, b'x');
            b
        },
    })
    .await;
    let rig = Rig::new(&server, &config(64 * 1024 * 1024), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/big.html"), "10.1234_bightml", None)
        .await
        .unwrap_err();
    assert!(matches!(err, PaperError::NotPdf { .. }), "{err}");
}

#[tokio::test]
async fn a_redirect_to_the_metadata_address_is_refused_on_that_hop() {
    let server = FakeServer::start(|path| match path {
        "/start" => Route::Redirect("http://169.254.169.254/latest/meta-data/".into()),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/start"), "10.1234_ssrf", None)
        .await
        .unwrap_err();

    match &err {
        PaperError::UnsafeUrl { reason, .. } => {
            assert!(reason.contains("169.254.169.254"), "{reason}")
        }
        other => panic!("expected UnsafeUrl, got {other}"),
    }
    assert_eq!(server.hits(), vec!["/start".to_string()]);
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn every_redirect_hop_is_revalidated_not_just_the_first_and_last() {
    // loopback -> loopback -> loopback -> RFC 1918: the allowed hops are
    // followed, the private one is refused.
    let server = FakeServer::start(|path| match path {
        "/a" => Route::Redirect("/b".into()),
        "/b" => Route::Redirect("/c".into()),
        "/c" => Route::Redirect("http://10.1.2.3/paper.pdf".into()),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/a"), "10.1234_hops", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::UnsafeUrl { .. }), "{err}");
    assert_eq!(server.hits(), vec!["/a", "/b", "/c"]);
}

#[tokio::test]
async fn redirects_to_other_schemes_and_endless_loops_are_refused() {
    let server = FakeServer::start(|path| match path {
        "/file" => Route::Redirect("file:///etc/passwd".into()),
        "/loop" => Route::Redirect("/loop".into()),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig
        .dl
        .download_by_url(&server.url("/file"), "10.1234_f", None)
        .await
        .unwrap_err();
    // reqwest never follows a `file:` Location: the 302 itself comes back as
    // the final response, which is not a PDF. Either way nothing is read from
    // disk and nothing is stored.
    assert!(
        matches!(
            err,
            PaperError::UnsafeUrl { .. } | PaperError::NotPdf { .. }
        ),
        "{err}"
    );
    assert!(rig.stored().await.is_empty());

    let err = rig
        .dl
        .download_by_url(&server.url("/loop"), "10.1234_l", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, PaperError::UnsafeUrl { reason, .. } if reason.contains("redirects")),
        "{err}"
    );
    // MAX_REDIRECTS followed hops + the initial request.
    let loops = server.hits().iter().filter(|h| *h == "/loop").count();
    assert_eq!(loops, url_guard::MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn the_production_policy_refuses_a_loopback_server_without_contacting_it() {
    let server = FakeServer::start(|_| pdf_route(5000)).await;
    let rig = Rig::production(&config(1_000_000));

    let err = rig
        .dl
        .download_by_url(&server.url("/paper.pdf"), "10.1234_prod", None)
        .await
        .unwrap_err();

    assert!(matches!(err, PaperError::UnsafeUrl { .. }), "{err}");
    assert!(
        server.hits().is_empty(),
        "production code must never connect to loopback: {:?}",
        server.hits()
    );
}

#[tokio::test]
async fn the_production_policy_refuses_private_and_non_http_urls_before_any_io() {
    let rig = Rig::production(&config(1_000_000));
    for url in [
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        "http://10.0.0.1/paper.pdf",
        "https://192.168.1.1/paper.pdf",
        "http://[::1]:7433/health",
        "http://[::ffff:7f00:1]/",
        "http://localhost:7445/mcp",
        "file:///home/user/.home-still/config.yaml",
        "ftp://example.org/paper.pdf",
        "https://user:pass@example.org/paper.pdf",
        "not a url",
    ] {
        let err = rig
            .dl
            .download_by_url(url, "10.1234_refused", None)
            .await
            .unwrap_err();
        assert!(matches!(err, PaperError::UnsafeUrl { .. }), "{url}: {err}");
    }
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn hostile_stems_are_rejected_before_any_request() {
    let server = FakeServer::start(|_| pdf_route(5000)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    for stem in ["", ".", "..", "a/b", "a\\b", "../../etc/passwd", "a\0b"] {
        let err = rig
            .dl
            .download_by_url(&server.url("/x.pdf"), stem, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PaperError::InvalidInput(_)),
            "{stem:?}: {err}"
        );
    }
    assert!(server.hits().is_empty());
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn an_already_stored_paper_is_skipped_without_a_request() {
    let server = FakeServer::start(|_| pdf_route(5000)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    rig.storage
        .put("papers/10/10.1234_have.pdf", pdf(300))
        .await
        .unwrap();

    let result = rig
        .dl
        .download_by_url(&server.url("/x.pdf"), "10.1234_have", None)
        .await
        .unwrap();

    assert!(result.skipped);
    assert_eq!(result.size_bytes, 300);
    assert!(server.hits().is_empty());
}

#[tokio::test]
async fn an_http_error_status_is_an_error_not_a_stored_body() {
    let server = FakeServer::start(|_| Route::Status(503)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let err = rig
        .dl
        .download_by_url(&server.url("/x.pdf"), "10.1234_503", None)
        .await
        .unwrap_err();
    assert!(matches!(err, PaperError::Http(_)), "{err}");
    assert!(rig.stored().await.is_empty());
}

// ---------------------------------------------------------------------------
// RA-38: the source chain tells local failures from per-source failures
// ---------------------------------------------------------------------------

fn arxiv_doi() -> &'static str {
    "10.48550/arXiv.2005.11401"
}

#[tokio::test]
async fn a_storage_write_failure_aborts_the_chain_with_the_real_error() {
    let server = FakeServer::start(|path| {
        if path.starts_with("/arxiv/pdf/") {
            pdf_route(4000)
        } else {
            Route::Status(500)
        }
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FlakyStorage {
        inner: LocalFsStorage::new(tmp.path()),
        fail_put: true,
        fail_head: false,
    });
    let dl = PaperDownloader::for_tests(
        storage,
        Arc::new(NoOpBus),
        &config(1_000_000),
        vec![provider_returning("s2", vec![server.url("/other.pdf")])],
        &server.base,
    );

    let err = dl.download_by_doi(arxiv_doi()).await.unwrap_err();

    assert!(matches!(err, PaperError::Storage(_)), "{err}");
    assert!(err.to_string().contains("disk full"), "{err}");
    // It stopped at the arXiv hit: Unpaywall, PMC and the provider were never asked.
    assert_eq!(server.hits(), vec!["/arxiv/pdf/2005.11401".to_string()]);
}

#[tokio::test]
async fn a_storage_head_failure_aborts_before_any_request() {
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FlakyStorage {
        inner: LocalFsStorage::new(tmp.path()),
        fail_put: false,
        fail_head: true,
    });
    let dl = PaperDownloader::for_tests(
        storage,
        Arc::new(NoOpBus),
        &config(1_000_000),
        vec![],
        &server.base,
    );

    let err = dl.download_by_doi("10.1234/abc").await.unwrap_err();

    assert!(matches!(err, PaperError::Storage(_)), "{err}");
    assert!(err.to_string().contains("connection reset"), "{err}");
    assert!(server.hits().is_empty(), "{:?}", server.hits());
}

#[tokio::test]
async fn a_local_failure_is_never_reported_as_no_open_access_pdf() {
    // The pre-fix behaviour: `if let Ok(..)` around every source turned a
    // failed write into "No open-access PDF found".
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FlakyStorage {
        inner: LocalFsStorage::new(tmp.path()),
        fail_put: true,
        fail_head: false,
    });
    let dl = PaperDownloader::for_tests(
        storage,
        Arc::new(NoOpBus),
        &config(1_000_000),
        vec![provider_returning("s2", vec![server.url("/p.pdf")])],
        &server.base,
    );
    let err = dl.download_by_doi("10.1234/abc").await.unwrap_err();
    assert!(!matches!(err, PaperError::NoSourceYielded { .. }), "{err}");
    assert!(
        !err.to_string().contains("No PDF could be downloaded"),
        "{err}"
    );
}

#[tokio::test]
async fn when_every_source_fails_the_error_lists_each_outcome() {
    let server = FakeServer::start(|path| {
        if path.starts_with("/unpaywall/") {
            Route::Status(503)
        } else if path.starts_with("/idconv/") {
            Route::Body {
                content_type: "application/json",
                body: br#"{"status":"ok","records":[{"requested-id":"10.1234/abc"}]}"#.to_vec(),
            }
        } else if path == "/landing" {
            html_route()
        } else if path == "/gone.pdf" {
            Route::Status(404)
        } else {
            Route::Status(500)
        }
    })
    .await;
    let resolvers: Vec<Arc<dyn PaperProvider>> = vec![
        Arc::new(FakeProvider {
            name: "semantic_scholar",
            reply: Box::new(|| Ok(None)),
        }),
        Arc::new(FakeProvider {
            name: "europe_pmc",
            reply: Box::new(|| {
                Err(PaperError::RateLimited {
                    provider: "europe_pmc".into(),
                    retry_after: None,
                })
            }),
        }),
        provider_returning(
            "openalex",
            vec![server.url("/landing"), server.url("/gone.pdf")],
        ),
        Arc::new(FakeProvider {
            name: "crossref",
            reply: Box::new(|| Ok(Some(paper_with("c", Some("10.1234/abc"), vec![])))),
        }),
    ];
    let rig = Rig::new(&server, &config(1_000_000), resolvers);

    let err = rig.dl.download_by_doi("10.1234/ABC").await.unwrap_err();

    let PaperError::NoSourceYielded { doi, sources } = &err else {
        panic!("expected NoSourceYielded, got {err}");
    };
    assert_eq!(doi, "10.1234/ABC");
    let find = |needle: &str| {
        sources
            .iter()
            .find(|s| s.source.contains(needle))
            .unwrap_or_else(|| panic!("no outcome for {needle}: {sources:?}"))
    };
    assert_eq!(find("unpaywall").kind, OutcomeKind::Failed);
    assert!(find("unpaywall").detail.contains("503"));
    assert_eq!(find("pmc").kind, OutcomeKind::NoCopy);
    assert!(find("pmc").detail.contains("not in PubMed Central"));
    assert_eq!(find("semantic_scholar").kind, OutcomeKind::NoCopy);
    assert_eq!(find("europe_pmc").kind, OutcomeKind::Failed);
    assert!(find("europe_pmc").detail.contains("Rate limited"));
    let landing = sources
        .iter()
        .find(|s| s.source.starts_with("openalex ") && s.source.ends_with("/landing"))
        .unwrap_or_else(|| panic!("no openalex /landing outcome: {sources:?}"));
    assert_eq!(landing.kind, OutcomeKind::NoCopy);
    assert!(landing.detail.contains("HTML"));
    assert_eq!(find("/gone.pdf").kind, OutcomeKind::NoCopy);
    assert_eq!(find("crossref").kind, OutcomeKind::NoCopy);
    assert!(find("crossref").detail.contains("no download URL"));

    // The rendered message carries every source, and a mix of failure kinds
    // is retryable, not "not found".
    let msg = err.to_string();
    for needle in [
        "unpaywall",
        "pmc",
        "semantic_scholar",
        "europe_pmc",
        "openalex",
        "crossref",
    ] {
        assert!(msg.contains(needle), "{needle} missing from: {msg}");
    }
    assert!(matches!(
        err.category(),
        crate::error::ErrorCategory::Transient
    ));
    assert!(rig.stored().await.is_empty());
}

#[tokio::test]
async fn when_no_source_failed_only_lacked_a_copy_the_error_is_permanent() {
    let server = FakeServer::start(|path| {
        if path.starts_with("/idconv/") {
            Route::Body {
                content_type: "application/json",
                body: br#"{"records":[{}]}"#.to_vec(),
            }
        } else {
            Route::Status(404)
        }
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let err = rig.dl.download_by_doi("10.1234/abc").await.unwrap_err();
    assert!(
        matches!(err.category(), crate::error::ErrorCategory::Permanent),
        "{err}"
    );
}

#[tokio::test]
async fn a_later_source_succeeds_after_earlier_sources_fail() {
    let server = FakeServer::start(|path| match path {
        p if p.starts_with("/unpaywall/") => Route::Status(500),
        p if p.starts_with("/idconv/") => Route::Status(500),
        "/direct.pdf" => pdf_route(6000),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(
        &server,
        &config(1_000_000),
        vec![provider_returning(
            "crossref",
            vec![server.url("/direct.pdf")],
        )],
    );

    let result = rig.dl.download_by_doi("10.1234/abc").await.unwrap();

    assert!(!result.skipped);
    assert_eq!(result.file_path, PathBuf::from("papers/10/10.1234_abc.pdf"));
    assert_eq!(
        rig.stored().await,
        vec!["papers/10/10.1234_abc.pdf".to_string()]
    );
}

#[tokio::test]
async fn a_landing_page_url_is_skipped_for_the_direct_pdf_listed_after_it() {
    // BACKLOG P1-14: a record listing the repository record page and the
    // direct PDF must end up with the PDF, and no HTML object.
    let server = FakeServer::start(|path| match path {
        "/handle/123" => html_route(),
        "/bitstream/000751721.pdf" => pdf_route(7000),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(
        &server,
        &config(1_000_000),
        vec![provider_returning(
            "openalex",
            vec![
                server.url("/handle/123"),
                server.url("/bitstream/000751721.pdf"),
            ],
        )],
    );

    let result = rig
        .dl
        .download_by_doi("10.1109/TVCG.2009.113")
        .await
        .unwrap();

    assert_eq!(
        result.file_path,
        PathBuf::from("papers/10/10.1109_tvcg.2009.113.pdf")
    );
    assert_eq!(
        rig.stored().await,
        vec!["papers/10/10.1109_tvcg.2009.113.pdf".to_string()]
    );
}

#[tokio::test]
async fn unpaywall_needs_a_contact_email_and_says_so_in_the_outcomes() {
    let server = FakeServer::start(|_| Route::Status(404)).await;
    let mut cfg = config(1_000_000);
    cfg.unpaywall_email = None;
    let rig = Rig::new(&server, &cfg, vec![]);

    let err = rig.dl.download_by_doi("10.1234/abc").await.unwrap_err();

    let PaperError::NoSourceYielded { sources, .. } = &err else {
        panic!("{err}");
    };
    let unpaywall = sources.iter().find(|s| s.source == "unpaywall").unwrap();
    assert!(
        unpaywall.detail.contains("unpaywall_email"),
        "{unpaywall:?}"
    );
    assert!(
        !server.hits().iter().any(|h| h.starts_with("/unpaywall/")),
        "Unpaywall must not be queried without an email: {:?}",
        server.hits()
    );
}

#[tokio::test]
async fn unpaywall_locations_are_tried_best_first_and_landing_pages_are_not_candidates() {
    // `pdfs` hosts the files; `api` plays Unpaywall and links to them.
    let pdfs = FakeServer::start(|path| match path {
        "/second.pdf" => pdf_route(5000),
        _ => Route::Status(404),
    })
    .await;
    let base = pdfs.base.clone();
    let api = FakeServer::start(move |path| {
        if path.starts_with("/unpaywall/") {
            Route::Body {
                content_type: "application/json",
                body: format!(
                    r#"{{"is_oa": true,
                        "best_oa_location": {{"url_for_pdf": null, "url_for_landing_page": "{base}/landing"}},
                        "oa_locations": [
                            {{"url_for_pdf": "{base}/first.pdf"}},
                            {{"url_for_pdf": "{base}/second.pdf"}}]}}"#
                )
                .into_bytes(),
            }
        } else {
            Route::Status(404)
        }
    })
    .await;
    let rig = Rig::new(&api, &config(1_000_000), vec![]);

    let result = rig.dl.download_by_doi("10.1234/abc").await.unwrap();

    assert!(!result.skipped);
    // First location 404s, second is stored; the landing page was never fetched.
    assert_eq!(pdfs.hits(), vec!["/first.pdf", "/second.pdf"]);
}

#[tokio::test]
async fn doi_and_contact_email_are_percent_encoded_in_lookup_urls() {
    let server = FakeServer::start(|_| Route::Status(404)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let _ = rig.dl.download_by_doi("10.1234/a b?c#d&e=f").await;

    let hits = server.hits();
    let unpaywall = hits
        .iter()
        .find(|h| h.starts_with("/unpaywall/"))
        .unwrap_or_else(|| panic!("{hits:?}"));
    // `?` and `#` in the DOI must not start a query/fragment; the email is
    // encoded in the one real query string.
    assert!(
        unpaywall.starts_with("/unpaywall/v2/10.1234/a%20b%3Fc%23d&e=f?email=ops%40example.org"),
        "{unpaywall}"
    );
    let idconv = hits
        .iter()
        .find(|h| h.starts_with("/idconv/"))
        .unwrap_or_else(|| panic!("{hits:?}"));
    assert!(
        idconv.contains("ids=10.1234%2Fa+b%3Fc%23d%26e%3Df"),
        "{idconv}"
    );
    assert!(idconv.contains("email=ops%40example.org"), "{idconv}");
}

#[tokio::test]
async fn pmc_sends_no_email_when_none_is_configured_and_never_an_invented_one() {
    let server = FakeServer::start(|_| Route::Status(404)).await;
    let mut cfg = config(1_000_000);
    cfg.unpaywall_email = None;
    let rig = Rig::new(&server, &cfg, vec![]);

    let _ = rig.dl.download_by_doi("10.1234/abc").await;

    let hits = server.hits();
    let idconv = hits.iter().find(|h| h.starts_with("/idconv/")).unwrap();
    assert!(idconv.contains("tool=home-still"), "{idconv}");
    assert!(!idconv.contains("email"), "{idconv}");
    assert!(!idconv.contains("example"), "{idconv}");
}

#[tokio::test]
async fn a_hostile_pmcid_in_the_response_is_not_followed() {
    let server = FakeServer::start(|path| {
        if path.starts_with("/idconv/") {
            Route::Body {
                content_type: "application/json",
                body: br#"{"records":[{"pmcid":"../../admin"}]}"#.to_vec(),
            }
        } else {
            Route::Status(404)
        }
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);

    let err = rig.dl.download_by_doi("10.1234/abc").await.unwrap_err();

    let PaperError::NoSourceYielded { sources, .. } = &err else {
        panic!("{err}");
    };
    let pmc = sources.iter().find(|s| s.source == "pmc").unwrap();
    assert_eq!(pmc.kind, OutcomeKind::Failed);
    assert!(!server.hits().iter().any(|h| h.starts_with("/pmc/")));
}

#[tokio::test]
async fn malformed_dois_are_rejected_before_any_source_is_asked() {
    let server = FakeServer::start(|_| Route::Status(404)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    for doi in [
        "",
        "not-a-doi",
        "10.1234",
        "10.1234/../../etc",
        "10.1234/a\\b",
        "doi:",
    ] {
        let err = rig.dl.download_by_doi(doi).await.unwrap_err();
        assert!(matches!(err, PaperError::InvalidInput(_)), "{doi:?}: {err}");
    }
    assert!(server.hits().is_empty());
}

#[tokio::test]
async fn arxiv_doi_uses_the_arxiv_pdf_url_for_any_case_of_the_prefix() {
    for doi in [
        "10.48550/arXiv.2005.11401",
        "10.48550/ARXIV.2005.11401",
        "https://doi.org/10.48550/arxiv.2005.11401",
    ] {
        let server = FakeServer::start(|path| {
            if path == "/arxiv/pdf/2005.11401" {
                pdf_route(4000)
            } else {
                Route::Status(404)
            }
        })
        .await;
        let rig = Rig::new(&server, &config(1_000_000), vec![]);
        let result = rig.dl.download_by_doi(doi).await.unwrap();
        assert_eq!(
            result.file_path,
            PathBuf::from("papers/10/10.48550_arxiv.2005.11401.pdf"),
            "{doi}"
        );
        assert_eq!(server.hits(), vec!["/arxiv/pdf/2005.11401".to_string()]);
    }
}

#[test]
fn arxiv_ids_that_are_not_arxiv_ids_are_not_turned_into_urls() {
    for id in [
        "",
        "../x",
        "a/../b",
        "a/b/c",
        "1234.5678?x=1",
        "1234.5678#f",
        "a b",
        "é",
    ] {
        assert!(
            arxiv_pdf_url("https://arxiv.org", id).is_err(),
            "{id:?} must be rejected"
        );
    }
    assert_eq!(
        arxiv_pdf_url("https://arxiv.org", "hep-th/9901001v2")
            .unwrap()
            .as_str(),
        "https://arxiv.org/pdf/hep-th/9901001v2"
    );
}

// ---------------------------------------------------------------------------
// RA-39: one storage identity per paper
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_search_download_and_a_doi_download_of_one_paper_share_one_object() {
    let server = FakeServer::start(|path| match path {
        "/p.pdf" => pdf_route(4000),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let service: Arc<dyn DownloadService> = Arc::new(rig.dl);

    // OpenAlex-shaped hit: provider id + a DOI spelled the way the API does.
    let paper = paper_with(
        "W2102450255",
        Some("10.1017/S0033291718004038"),
        vec![server.url("/p.pdf")],
    );
    let batch = download_batch(service.clone(), vec![paper], 2, None)
        .await
        .unwrap();
    assert_eq!(batch.failed.len(), 0, "{:?}", batch.failed);
    assert_eq!(batch.succeeded.len(), 1);
    assert_eq!(
        batch.succeeded[0].file_path,
        PathBuf::from("papers/10/10.1017_s0033291718004038.pdf")
    );

    // The same paper requested by DOI — different case, resolver prefix —
    // finds the stored object instead of creating a second stem.
    for doi in [
        "10.1017/s0033291718004038",
        "https://doi.org/10.1017/S0033291718004038",
        "DOI:10.1017/S0033291718004038",
    ] {
        let again = service.download_by_doi(doi).await.unwrap();
        assert!(again.skipped, "{doi}");
        assert_eq!(again.file_path, batch.succeeded[0].file_path);
    }
}

#[tokio::test]
async fn papers_without_a_doi_keep_their_case_sensitive_id_stem() {
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let storage = rig.storage.clone();
    let service: Arc<dyn DownloadService> = Arc::new(rig.dl);
    let batch = download_batch(
        service,
        vec![paper_with("W2102450255", None, vec![server.url("/p.pdf")])],
        1,
        None,
    )
    .await
    .unwrap();
    assert_eq!(batch.succeeded.len(), 1, "{:?}", batch.failed);
    assert!(storage
        .head("papers/W2/W2102450255.pdf")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn hostile_paper_ids_fail_the_paper_and_store_nothing() {
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let storage = rig.storage.clone();
    let service: Arc<dyn DownloadService> = Arc::new(rig.dl);
    for id in ["..", ".", "", "  ", "a\0b"] {
        let batch = download_batch(
            service.clone(),
            vec![paper_with(id, None, vec![server.url("/p.pdf")])],
            1,
            None,
        )
        .await
        .unwrap();
        assert_eq!(batch.failed.len(), 1, "id {id:?}");
        assert!(batch.succeeded.is_empty(), "id {id:?}");
    }
    assert!(storage.list("").await.unwrap().is_empty());
    assert!(
        server.hits().is_empty(),
        "no request for an unaddressable paper"
    );
}

#[tokio::test]
async fn a_batch_with_zero_concurrency_is_an_error_not_a_hang() {
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let service: Arc<dyn DownloadService> = Arc::new(rig.dl);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        download_batch(service, vec![paper_with("a", None, vec![])], 0, None),
    )
    .await
    .expect("download_batch(0) must return, not hang");
    assert!(matches!(result, Err(PaperError::InvalidInput(_))));
}

// ---------------------------------------------------------------------------
// RA-38 in the batch service: local failure ends the paper
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_batch_paper_stops_on_a_storage_failure_instead_of_trying_the_doi_path() {
    let server = FakeServer::start(|_| pdf_route(4000)).await;
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FlakyStorage {
        inner: LocalFsStorage::new(tmp.path()),
        fail_put: true,
        fail_head: false,
    });
    let dl = PaperDownloader::for_tests(
        storage,
        Arc::new(NoOpBus),
        &config(1_000_000),
        vec![],
        &server.base,
    );
    let service: Arc<dyn DownloadService> = Arc::new(dl);
    let paper = paper_with(
        "W1",
        Some("10.1234/abc"),
        vec![server.url("/one.pdf"), server.url("/two.pdf")],
    );

    let batch = download_batch(service, vec![paper], 1, None).await.unwrap();

    assert_eq!(batch.failed.len(), 1);
    assert!(
        batch.failed[0].error.contains("disk full"),
        "{}",
        batch.failed[0].error
    );
    assert_eq!(
        server.hits(),
        vec!["/one.pdf".to_string()],
        "second URL and the DOI chain must not run after a disk failure"
    );
}

#[tokio::test]
async fn a_batch_failure_names_every_attempt() {
    let server = FakeServer::start(|path| match path {
        "/landing" => html_route(),
        _ => Route::Status(404),
    })
    .await;
    let rig = Rig::new(&server, &config(1_000_000), vec![]);
    let service: Arc<dyn DownloadService> = Arc::new(rig.dl);
    let paper = paper_with(
        "W1",
        Some("10.1234/abc"),
        vec![server.url("/landing"), server.url("/missing.pdf")],
    );

    let batch = download_batch(service, vec![paper], 1, None).await.unwrap();

    let msg = &batch.failed[0].error;
    assert!(
        msg.contains("/landing") && msg.contains("not a PDF"),
        "{msg}"
    );
    assert!(msg.contains("/missing.pdf"), "{msg}");
    assert!(msg.contains("DOI 10.1234/abc"), "{msg}");
}
