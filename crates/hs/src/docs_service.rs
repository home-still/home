//! The docs service: serving the corpus's documents over signed,
//! expiring links.
//!
//! A search hit names a document the user cannot open. This service is
//! what turns that hit into a link a browser can follow: it reads the
//! document out of the corpus's storage and streams it back, having
//! first checked that the request carries a genuine, unexpired
//! signature for that document.
//!
//! # One read path
//!
//! The bytes come from the same [`Storage`] trait everything else reads
//! through, so the service serves exactly what the pipeline wrote and
//! nothing that only exists on this host's disk.
//!
//! # The client never names a file
//!
//! A request names a *stem*. The service decides where that stem's
//! source object lives, by probing the same `pdf` → `html` → `epub`
//! order the MCP server uses. A client that asks for
//! `papers/10/whatever.pdf` is refused: it asked for a storage key, and
//! storage keys are not a public interface.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::stream::BoxStream;
use futures::StreamExt;
use hs_common::docs::{LinkFormat, LinkKey, LinkQuery, Revocations};
use hs_common::storage::{ByteRange, Storage};

/// The extensions a source object may have, in probe order.
///
/// The same order the MCP server's `find_source_key` uses, so a link
/// and a tool agree on which file a stem means.
const SOURCE_EXTENSIONS: [&str; 3] = ["pdf", "html", "epub"];

/// The separator between the pages of a converted markdown.
///
/// It is **reserved** by the scribe pipeline
/// (`crates/hs-scribe/src/pipeline/markdown_generator.rs`): a `---` a
/// model emits inside a page is rewritten to `***` first, so this exact
/// byte sequence only ever appears between two pages. The markdown's
/// page N is therefore the source document's page N.
const PAGE_SEPARATOR: &str = "\n\n---\n\n";

/// What the service needs to answer a request.
pub struct DocsState {
    /// The corpus's storage.
    pub storage: Arc<dyn Storage>,
    /// The key links are signed with.
    pub key: LinkKey,
    /// The subjects whose links are revoked.
    pub revocations: Revocations,
    /// The prefix the source objects live under.
    pub papers_prefix: String,
    /// The prefix the markdown lives under.
    pub markdown_prefix: String,
}

/// Build the service's router.
pub fn router(state: Arc<DocsState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/docs/v1/{stem}/source", get(serve_source))
        .route("/docs/v1/{stem}/markdown", get(serve_markdown))
        .layer(axum::middleware::from_fn(catch_panic))
        .with_state(state)
}

/// A handler panic is a 500 and the server keeps serving.
async fn catch_panic(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    hs_common::panic_guard::http::catch_panic(request, next).await
}

/// The service's health: it is up if it can read the corpus.
async fn health(State(state): State<Arc<DocsState>>) -> Response {
    // A health check that never touches storage cannot tell a working
    // service from one whose backend is gone.
    let probe = "markdown";
    let ok = state.storage.list(probe).await.is_ok();
    if ok {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "storage unreachable").into_response()
    }
}

/// The refusal a bad link gets.
///
/// Every refusal is a 403 with the same body, so a client learns
/// nothing about whether the stem exists, whether the link was expired
/// or whether the signature was forged.
fn refused() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "link refused",
    )
        .into_response()
}

/// A stem that is not a stem is a 400, not a 403: the request is
/// malformed, and saying so leaks nothing about the corpus.
fn bad_request() -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "bad request",
    )
        .into_response()
}

/// The document's source object.
async fn serve_source(
    State(state): State<Arc<DocsState>>,
    Path(stem): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    range: RequestedRange,
) -> Response {
    serve(state, stem, query, range, LinkFormat::Source).await
}

/// The document's markdown.
async fn serve_markdown(
    State(state): State<Arc<DocsState>>,
    Path(stem): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    range: RequestedRange,
) -> Response {
    serve(state, stem, query, range, LinkFormat::Markdown).await
}

/// Serve one document.
async fn serve(
    state: Arc<DocsState>,
    stem: String,
    query: HashMap<String, String>,
    requested_range: RequestedRange,
    format: LinkFormat,
) -> Response {
    // The stem is the client's input and it names a file, so it is
    // validated before anything else happens. `validate_stem` rejects
    // `..`, absolute paths, NUL and backslashes.
    if hs_common::validate_stem(&stem).is_err() {
        return bad_request();
    }
    // The format in the query must match the route: a link signed for
    // the markdown must not open the source.
    let Ok(params) = LinkQuery::parse(&encode_query(&query)) else {
        return refused();
    };
    if params.format != format {
        return refused();
    }
    if hs_common::docs::verify(
        &state.key,
        &hs_common::docs::SignedFields {
            stem: stem.clone(),
            format,
            subject: params.subject.clone(),
            page: params.page,
            issued_at: params.issued_at,
            expires_at: params.expires_at,
        },
        &params.signature,
    )
    .is_err()
    {
        return refused();
    }
    // A link minted before its subject was revoked is refused. The
    // link carries its issue time, so a subject that was revoked and
    // then re-enrolled gets fresh links.
    if state
        .revocations
        .is_revoked(&params.subject, params.issued_at)
    {
        return refused();
    }

    // The key: where this stem's object lives. The client never says.
    // `body` is `Some` when the link names a page of the markdown: that
    // page is served in place of the whole document.
    let (key, content_type, extension, body) = match format {
        LinkFormat::Source => {
            match find_source_key(&*state.storage, &state.papers_prefix, &stem).await {
                Some((key, content_type, extension)) => (key, content_type, extension, None),
                None => return not_found(),
            }
        }
        LinkFormat::Markdown => {
            let key = format!(
                "{}/{}",
                state.markdown_prefix.trim_end_matches('/'),
                hs_common::sharded_key(&stem, "md")
            );
            if state.storage.head(&key).await.ok().flatten().is_none() {
                return not_found();
            }
            // A link that names a page serves that page: the markdown's
            // pages are the source's pages, and the separator between
            // them is a reserved byte sequence, so splitting on it is
            // splitting on the document's own page boundaries.
            let body = match params.page {
                None => None,
                Some(page) => {
                    let bytes = match state.storage.get(&key).await {
                        Ok(bytes) => bytes,
                        Err(e) if hs_common::storage::is_not_found(&e) => return not_found(),
                        Err(_) => return not_found(),
                    };
                    let text = String::from_utf8_lossy(&bytes);
                    let pages: Vec<&str> = text.split(PAGE_SEPARATOR).collect();
                    // The pages are 1-based. A page the document does
                    // not have is a 404, not an empty page: the link
                    // named something that is not there.
                    if page == 0 || page as usize > pages.len() {
                        return not_found();
                    }
                    Some(pages[page as usize - 1].to_string())
                }
            };
            (
                key,
                "text/markdown; charset=utf-8".to_string(),
                "md".to_string(),
                body,
            )
        }
    };

    let Some(meta) = state.storage.head(&key).await.ok().flatten() else {
        return not_found();
    };

    // The range the client asked for, if any. A PDF viewer seeking to a
    // page sends one.
    let range = requested_range.0.as_deref().and_then(ByteRange::parse);

    // A range that the object cannot satisfy is a 416, not a 403: the
    // link was good, the range was not.
    if let Some(range) = range {
        if range.len(meta.size).is_none() {
            return range_not_satisfiable(meta.size);
        }
    }

    // The bytes that are sent: the whole object, the range's part, or
    // the one page the link named.
    let (size, stream): (u64, BoxStream<'static, anyhow::Result<Vec<u8>>>) = match body {
        // A page of the markdown was already extracted.
        Some(page) => {
            let bytes = page.into_bytes();
            let len = bytes.len() as u64;
            let stream = futures::stream::once(async move { Ok(bytes) }).boxed();
            (len, stream)
        }
        None => match state.storage.get_stream(&key, range).await {
            Ok(pair) => pair,
            Err(e) if hs_common::storage::is_not_found(&e) => return not_found(),
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "the document could not be read",
                )
                    .into_response()
            }
        },
    };

    // The response's own length: the whole object, the range's part, or
    // the page.
    let body_len = match range {
        Some(range) => range.len(size).unwrap_or(0),
        None => size,
    };
    let status = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    let filename = filename_for(&stem, &extension);
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("inline")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body_len.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(etag) = meta.etag.as_ref() {
        if let Ok(value) = HeaderValue::from_str(etag) {
            headers.insert(header::ETAG, value);
        }
    }
    if let Some(modified) = meta.last_modified {
        if let Ok(value) = HeaderValue::from_str(&httpdate(modified)) {
            headers.insert(header::LAST_MODIFIED, value);
        }
    }
    if let Some(range) = range {
        let start = range.start.unwrap_or(0);
        let end = start + body_len.saturating_sub(1);
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                .unwrap_or(HeaderValue::from_static("bytes 0-0/0")),
        );
    }
    // Where in the document the bytes that were sent are. A client
    // that fetched a page knows which page it is looking at, and a
    // client that fetched the whole document can point at the page the
    // link named.
    if let Some(page) = params.page {
        if let Ok(value) = HeaderValue::from_str(&format!("#page={page}")) {
            headers.insert(header::CONTENT_LOCATION, value);
        }
    }
    // An HTML source is an untrusted page: it is served in a sandbox,
    // with no scripts and no same-origin access.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // A link expires, so the response must not outlive it in a cache.
    let remaining = params
        .expires_at
        .saturating_sub(hs_common::docs::now_secs());
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&format!("private, max-age={remaining}"))
            .unwrap_or(HeaderValue::from_static("private, no-store")),
    );

    // A read failure mid-stream ends the response; the bytes already
    // sent are the bytes the client got. The error itself is not
    // interesting to the client and must not leak the storage layout.
    let body = Body::from_stream(stream.map(|chunk| {
        chunk.map_err(|e| std::io::Error::other(format!("the document could not be read: {e}")))
    }));
    (status, headers, body).into_response()
}

/// The `Range` header the client sent, if any.
#[derive(Clone, Default)]
pub struct RequestedRange(pub Option<String>);

impl<S> FromRequestParts<S> for RequestedRange
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string()),
        ))
    }
}

use axum::extract::FromRequestParts;

/// Find a stem's source object, probing the extensions in order.
///
/// Returns the key, the content type and the extension.
async fn find_source_key(
    storage: &dyn Storage,
    papers_prefix: &str,
    stem: &str,
) -> Option<(String, String, String)> {
    for ext in SOURCE_EXTENSIONS {
        let key = format!(
            "{}/{}",
            papers_prefix.trim_end_matches('/'),
            hs_common::sharded_key(stem, ext)
        );
        if storage.head(&key).await.ok().flatten().is_some() {
            let content_type = match ext {
                "pdf" => "application/pdf",
                "html" => "text/html; charset=utf-8",
                "epub" => "application/epub+zip",
                _ => "application/octet-stream",
            };
            return Some((key, content_type.to_string(), ext.to_string()));
        }
    }
    None
}

/// A filename for the `Content-Disposition` header.
///
/// The stem is a document id, and it is not necessarily safe to put in
/// a header verbatim, so everything that is not a letter, a digit, a
/// dot, a dash or an underscore is dropped.
fn filename_for(stem: &str, extension: &str) -> String {
    let base: String = stem
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(120)
        .collect();
    let base = if base.is_empty() {
        "document".to_string()
    } else {
        base
    };
    format!("{base}.{extension}")
}

/// The `Last-Modified` value, in the HTTP date format.
fn httpdate(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    httpdate_from_unix(secs)
}

/// Format a Unix timestamp as an HTTP date.
fn httpdate_from_unix(secs: i64) -> String {
    // Days since the epoch, then the civil date, then the time. The
    // arithmetic is done by hand so the crate needs no date library.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 was a Thursday, and the list starts with it.
    let weekday = ((days % 7) + 7) % 7;
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // The civil date from the days since the epoch (Howard Hinnant's
    // algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        hour,
        minute,
        second
    )
}

/// A document that does not exist.
///
/// The same 404 whether the stem is unknown or the object was never
/// written: a link for a document the corpus does not hold must not
/// distinguish the two.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "not found",
    )
        .into_response()
}

/// A range the object cannot satisfy.
fn range_not_satisfiable(size: u64) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{size}"))
            .unwrap_or(HeaderValue::from_static("bytes */0")),
    );
    (
        StatusCode::RANGE_NOT_SATISFIABLE,
        headers,
        "range not satisfiable",
    )
        .into_response()
}

/// Rebuild a query string from the parsed parameters.
///
/// The query is parsed into a map by axum, which loses the original
/// ordering; the signature covers the *values*, not the order, so the
/// map is enough.
fn encode_query(query: &HashMap<String, String>) -> String {
    query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;
    use hs_common::storage::LocalFsStorage;

    /// A service over a temporary directory, with a fixed key.
    async fn service(
        dir: &std::path::Path,
    ) -> (Router, hs_common::docs::LinkKey, std::sync::Arc<DocsState>) {
        let storage = Arc::new(LocalFsStorage::new(dir));
        let key = hs_common::docs::LinkKey::from_bytes(vec![11u8; 32]).unwrap();
        let state = Arc::new(DocsState {
            storage,
            key: key.clone(),
            revocations: Revocations::default(),
            papers_prefix: "papers".to_string(),
            markdown_prefix: "markdown".to_string(),
        });
        (router(state.clone()), key, state)
    }

    /// One request against the service, over a real loopback socket.
    ///
    /// The service is bound to an ephemeral port and driven with a real
    /// HTTP client, so the test exercises the same path a browser does:
    /// the headers, the status and the body are the wire's, not the
    /// router's internals.
    async fn call(app: &Router, uri: &str, range: Option<&str>) -> TestResponse {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app.clone();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let mut builder = client.get(format!("http://{addr}{uri}"));
        if let Some(range) = range {
            builder = builder.header("Range", range);
        }
        let resp = builder.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = resp.bytes().await.unwrap().to_vec();
        TestResponse {
            status,
            headers,
            body,
        }
    }

    /// What a request came back with.
    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestResponse {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// A document in the corpus: a PDF under `papers/` and its markdown.
    async fn put_document(dir: &std::path::Path, stem: &str, body: &[u8]) {
        let storage = LocalFsStorage::new(dir);
        storage
            .put(
                &format!("papers/{}", hs_common::sharded_key(stem, "pdf")),
                body.to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("markdown/{}", hs_common::sharded_key(stem, "md")),
                format!("# {stem}\n\nthe body\n").into_bytes(),
            )
            .await
            .unwrap();
    }

    /// The link for a document, as a client would hold it.
    fn link(
        key: &hs_common::docs::LinkKey,
        stem: &str,
        format: LinkFormat,
        subject: &str,
    ) -> String {
        let params = hs_common::docs::mint(key, stem, format, subject, None, 3600).unwrap();
        format!("/docs/v1/{stem}/{format}?{}", params.to_query())
    }

    #[tokio::test]
    async fn a_signed_link_serves_the_document() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let uri = link(&key, "10.1234_test", LinkFormat::Source, "laptop");
        let resp = call(&app, &uri, None).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("content-type"), Some("application/pdf"));
        assert_eq!(resp.header("accept-ranges"), Some("bytes"));
        assert_eq!(resp.header("content-security-policy"), Some("sandbox"));
        assert_eq!(resp.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(&resp.body[..5], b"%PDF-");
    }

    #[tokio::test]
    async fn a_ranged_request_gets_a_partial_response() {
        // A PDF viewer seeking to a page sends a Range; it must get a 206
        // with the right Content-Range, or the viewer cannot seek.
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"0123456789abcdefghij").await;

        let uri = link(&key, "10.1234_test", LinkFormat::Source, "laptop");
        let resp = call(&app, &uri, Some("bytes=4-9")).await;
        assert_eq!(resp.status, 206);
        assert_eq!(resp.header("content-range"), Some("bytes 4-9/20"));
        assert_eq!(&resp.body[..], b"456789");
    }

    #[tokio::test]
    async fn a_tampered_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let uri = link(&key, "10.1234_test", LinkFormat::Source, "laptop");
        // A different stem.
        let tampered = uri.replace("10.1234_test", "10.1234_other");
        assert_eq!(call(&app, &tampered, None).await.status, 403);
        // A different subject.
        let tampered = uri.replace("sub=laptop", "sub=someone-else");
        assert_eq!(call(&app, &tampered, None).await.status, 403);
        // A forged signature.
        let tampered = uri.replace(
            "sig=",
            "sig=0000000000000000000000000000000000000000000000000000000000000000&x=",
        );
        assert_eq!(call(&app, &tampered, None).await.status, 403);
        // No signature at all.
        assert_eq!(
            call(&app, "/docs/v1/10.1234_test/source", None)
                .await
                .status,
            403
        );
    }

    #[tokio::test]
    async fn an_expired_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        // A link whose expiry is in the past.
        let params =
            hs_common::docs::mint(&key, "10.1234_test", LinkFormat::Source, "laptop", None, 0)
                .unwrap();
        let uri = format!("/docs/v1/10.1234_test/source?{}", params.to_query());
        assert_eq!(call(&app, &uri, None).await.status, 403);
    }

    #[tokio::test]
    async fn a_link_for_one_format_does_not_open_the_other() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        // A link signed for the source, used on the markdown route.
        let uri = link(&key, "10.1234_test", LinkFormat::Source, "laptop");
        let swapped = uri.replace("/source?", "/markdown?");
        assert_eq!(call(&app, &swapped, None).await.status, 403);
    }

    #[tokio::test]
    async fn a_revoked_subject_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let uri = link(&key, "10.1234_test", LinkFormat::Source, "laptop");
        assert_eq!(call(&app, &uri, None).await.status, 200);

        // Revoking the subject invalidates its links.
        let mut revocations = state.revocations.clone();
        revocations.revoke("laptop", hs_common::docs::now_secs() + 1);
        let state = Arc::new(DocsState {
            storage: state.storage.clone(),
            key: state.key.clone(),
            revocations,
            papers_prefix: state.papers_prefix.clone(),
            markdown_prefix: state.markdown_prefix.clone(),
        });
        let app = router(state);
        assert_eq!(call(&app, &uri, None).await.status, 403);
    }

    #[tokio::test]
    async fn a_path_traversal_attempt_is_refused_before_storage_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        // A stem that tries to escape. The signature covers the stem,
        // so a valid signature for a traversal stem does not exist — but
        // the request must be refused on the stem, not on the
        // signature, and never reach storage.
        for stem in [
            "..%2F..%2Fetc%2Fpasswd",
            "..%2Fsecrets.env",
            "%2Fetc%2Fpasswd",
            "papers%2F10%2F10.1234_test.pdf",
        ] {
            let uri = format!("/docs/v1/{stem}/source?exp=1&sub=laptop&fmt=source&sig=ab");
            let resp = call(&app, &uri, None).await;
            assert_eq!(resp.status, 400, "{stem}");
        }
    }

    #[tokio::test]
    async fn a_document_the_corpus_does_not_hold_is_a_404() {
        // A valid link for a stem with no document: 404, not 403. The
        // link was good; the document is not there.
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        let uri = link(&key, "10.9999_missing", LinkFormat::Source, "laptop");
        assert_eq!(call(&app, &uri, None).await.status, 404);
    }

    #[tokio::test]
    async fn the_markdown_link_serves_the_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let uri = link(&key, "10.1234_test", LinkFormat::Markdown, "laptop");
        let resp = call(&app, &uri, None).await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("content-type"),
            Some("text/markdown; charset=utf-8")
        );
        assert!(String::from_utf8_lossy(&resp.body).starts_with("# 10.1234_test"));
    }

    #[tokio::test]
    async fn a_link_naming_a_page_serves_that_page() {
        // The markdown's pages are the source's pages, and the
        // separator between them is reserved, so a link that names
        // page 2 serves the second page and not the whole document.
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        // A two-page document.
        let storage = LocalFsStorage::new(tmp.path());
        storage
            .put(
                &format!("markdown/{}", hs_common::sharded_key("10.1234_test", "md")),
                b"page one text\n\n---\n\npage two text".to_vec(),
            )
            .await
            .unwrap();

        let params = hs_common::docs::mint(
            &key,
            "10.1234_test",
            LinkFormat::Markdown,
            "laptop",
            Some(2),
            3600,
        )
        .unwrap();
        let uri = format!("/docs/v1/10.1234_test/markdown?{}", params.to_query());
        let resp = call(&app, &uri, None).await;
        assert_eq!(resp.status, 200);
        let body = String::from_utf8_lossy(&resp.body).to_string();
        assert!(body.contains("page two text"), "{body}");
        assert!(!body.contains("page one text"), "{body}");
        // The response says where in the document the bytes are.
        assert_eq!(resp.header("content-location"), Some("#page=2"));
    }

    #[tokio::test]
    async fn a_link_naming_a_page_the_document_does_not_have_is_a_404() {
        // The link was good; the page is not there. That is a 404, not a
        // 403 and not an empty page.
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let params = hs_common::docs::mint(
            &key,
            "10.1234_test",
            LinkFormat::Markdown,
            "laptop",
            Some(99),
            3600,
        )
        .unwrap();
        let uri = format!("/docs/v1/10.1234_test/markdown?{}", params.to_query());
        assert_eq!(call(&app, &uri, None).await.status, 404);
    }

    #[tokio::test]
    async fn a_tampered_page_is_refused() {
        // The page is part of the request, so changing it in the URL
        // invalidates the signature: a link for page 2 cannot be
        // edited into a link for page 3.
        let tmp = tempfile::tempdir().unwrap();
        let (app, key, _state) = service(tmp.path()).await;
        put_document(tmp.path(), "10.1234_test", b"%PDF-1.5 pretend").await;

        let params = hs_common::docs::mint(
            &key,
            "10.1234_test",
            LinkFormat::Source,
            "laptop",
            Some(2),
            3600,
        )
        .unwrap();
        let uri = format!("/docs/v1/10.1234_test/source?{}", params.to_query());
        assert_eq!(call(&app, &uri, None).await.status, 200);
        // The same signature, a different page.
        let tampered = uri.replace("page=2", "page=3");
        assert_eq!(call(&app, &tampered, None).await.status, 403);
    }

    #[tokio::test]
    async fn a_filename_drops_anything_that_is_not_safe_in_a_header() {
        assert_eq!(filename_for("10.1234_abcd", "pdf"), "10.1234_abcd.pdf");
        // The parentheses, the colon and the space are all dropped.
        assert_eq!(
            filename_for("10.1002_(sici)1099-1050(199601)5:1_77", "pdf"),
            "10.1002_sici1099-105019960151_77.pdf"
        );
        // A stem with nothing usable in it still gets a name.
        assert_eq!(filename_for("///", "pdf"), "document.pdf");
    }

    #[test]
    fn an_http_date_is_the_expected_format() {
        assert_eq!(httpdate_from_unix(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            httpdate_from_unix(1_700_000_000),
            "Tue, 14 Nov 2023 22:13:20 GMT"
        );
    }
}
