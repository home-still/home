//! Word-box sidecars, built from `scribe.boxes` events.
//!
//! A `scribe.boxes` event names a PDF in a document store. The consumer
//! fetches that PDF, has a scribe server build its sidecar
//! (`POST /scribe/boxes`), and stores the sidecar under the store's boxes
//! prefix ([`build_and_store`]). Every build is requested through
//! [`request_boxes`]: by the conversion watcher when a PDF has converted
//! (and, for a PDF found already converted, when its sidecar is missing), by
//! personal ingest, and by `hs scribe boxes catch-up` for what [`survey`]
//! finds missing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;
use hs_common::event_bus::{specs, EventBus};
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, Stores};
use hs_common::word_boxes;
use serde::{Deserialize, Serialize};

use crate::classify::{failure_code, FailureCode};
use crate::client::ScribeClient;
use crate::event_watch::{
    blocking, parse_event_key, permanent, storage_failure, HandlerError, Keyed, MAX_SOURCE_BYTES,
};

/// The extension of the one source type that has word boxes.
pub const BOXED_SOURCE_EXTENSION: &str = "pdf";

/// The extension of a store's converted markdown objects.
const MARKDOWN_EXTENSION: &str = "md";

/// Payload of `scribe.boxes`: the storage key of the PDF whose sidecar is
/// to be built.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BoxesEvent {
    pub key: String,
}

impl Keyed for BoxesEvent {
    fn key(&self) -> &str {
        &self.key
    }
}

/// Ask for the word-box sidecar of the PDF at `source_key` by publishing
/// `scribe.boxes`. A publish that fails is `Transient`.
pub async fn request_boxes(bus: &dyn EventBus, source_key: &str) -> Result<(), HandlerError> {
    let event = BoxesEvent {
        key: source_key.to_string(),
    };
    let bytes = serde_json::to_vec(&event).map_err(|e| {
        HandlerError::Permanent(anyhow::Error::new(e).context("encoding scribe.boxes"))
    })?;
    bus.publish(specs::SCRIBE_BOXES.subject, &bytes)
        .await
        .map_err(|e| {
            HandlerError::Transient(
                e.context(format!("scribe.boxes publish failed for {source_key}")),
            )
        })
}

/// [`request_boxes`] for the PDF at `source_key`, unless its sidecar is
/// already stored. A key that is not a PDF is owed nothing. A PDF under no
/// store's source prefix has no sidecar key to look under: the request is
/// published, and the consumer refuses it with the reason.
pub async fn request_missing_boxes(
    bus: &dyn EventBus,
    storage: &dyn Storage,
    stores: &Stores,
    source_key: &str,
) -> Result<(), HandlerError> {
    let (stem, ext) = parse_event_key(source_key)?;
    if ext != BOXED_SOURCE_EXTENSION {
        return Ok(());
    }
    if let Some((_, layout)) = stores.for_source_key(source_key) {
        let boxes_key = word_boxes::boxes_key(layout, &stem);
        let stored = storage
            .exists(&boxes_key)
            .await
            .map_err(|e| storage_failure(e, format!("head({boxes_key}) failed")))?;
        if stored {
            return Ok(());
        }
    }
    request_boxes(bus, source_key).await
}

/// The scribe servers that build sidecars, taken in rotation.
///
/// A sidecar needs no VLM, so the servers are not chosen by the conversion
/// pool's readiness (which reports free VLM slots): every configured server
/// takes its turn, and a build falls through to the next server when one
/// cannot do it.
pub struct BoxServers {
    clients: Vec<ScribeClient>,
    next: AtomicUsize,
}

impl BoxServers {
    /// Fails when `clients` is empty: with no server nothing can be built.
    pub fn new(clients: Vec<ScribeClient>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !clients.is_empty(),
            "no scribe server to build word boxes on"
        );
        Ok(Self {
            clients,
            next: AtomicUsize::new(0),
        })
    }

    /// How many servers there are; never zero.
    pub fn count(&self) -> usize {
        self.clients.len()
    }

    /// Every server once, starting one past where the previous call started.
    fn in_turn(&self) -> impl Iterator<Item = &ScribeClient> {
        let count = self.clients.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % count;
        self.clients.iter().cycle().skip(start).take(count)
    }
}

/// Whether `reply` is the sidecar of `pdf`: it decodes, it is valid, and it
/// names the bytes that were sent.
fn check_sidecar(pdf: &[u8], reply: &[u8]) -> anyhow::Result<()> {
    let boxes = word_boxes::decode(reply).context("the reply is not a word-box sidecar")?;
    let sent = word_boxes::source_sha256(pdf);
    anyhow::ensure!(
        boxes.source_sha256 == sent,
        "the sidecar names source {} but the PDF sent is {sent}",
        boxes.source_sha256
    );
    Ok(())
}

/// The sidecar of `pdf`, from the first server in turn that builds it.
///
/// A server's verdict on the document (it is not a PDF, or cannot be read)
/// is final and `Permanent`. A server that cannot build (unreachable, an
/// older binary, no OCR models, a reply that is not this PDF's sidecar) is
/// passed over; when every server was, the failure is `Transient` and names
/// each server's reason.
async fn build_on_a_server(
    servers: &BoxServers,
    key: &str,
    pdf: &bytes::Bytes,
) -> Result<Vec<u8>, HandlerError> {
    let mut passed_over = Vec::new();
    for client in servers.in_turn() {
        let failure = match client.word_boxes(pdf.clone()).await {
            Ok(reply) => {
                let (sent, built) = (pdf.clone(), reply.clone());
                match blocking("word-box check", key, move || check_sidecar(&sent, &built)).await? {
                    Ok(()) => return Ok(reply),
                    Err(e) => e,
                }
            }
            Err(e) if failure_code(&e).is_some() => {
                return Err(HandlerError::Permanent(
                    e.context(format!("{key} has no word boxes")),
                ));
            }
            // Not a verdict on the document or on this server's ability:
            // the subscriber stops the whole watcher on it.
            Err(e) if crate::client::is_backend_unauthorized(&e) => {
                return Err(HandlerError::Transient(e));
            }
            Err(e) => e,
        };
        tracing::warn!(
            key = %key,
            server = %client.url(),
            error = %format!("{failure:#}"),
            "word boxes: this server did not build the sidecar"
        );
        passed_over.push(format!("{}: {failure:#}", client.url()));
    }
    Err(HandlerError::Transient(anyhow::anyhow!(
        "no scribe server built the word boxes of {key} ({})",
        passed_over.join("; ")
    )))
}

/// Build the sidecar `event` asks for and store it. Returns the sidecar's
/// storage key.
///
/// `Permanent` (the event is never redelivered): the key is not a PDF, is
/// under no store's source prefix, or is not where its store keeps that
/// stem's PDF — a sidecar describes the PDF a link opens, and that is the
/// stored one; the PDF is gone or over the size limit; a server refused it
/// as unreadable. `Transient`: storage and server trouble.
pub async fn build_and_store(
    storage: &dyn Storage,
    stores: &Stores,
    servers: &BoxServers,
    event: &BoxesEvent,
) -> Result<String, HandlerError> {
    let key = event.key.as_str();
    let (stem, ext) = parse_event_key(key)?;
    if ext != BOXED_SOURCE_EXTENSION {
        return Err(permanent(
            FailureCode::UnsupportedExtension,
            format!("{key} is not a PDF: only a PDF has word boxes"),
        ));
    }
    let Some((store, layout)) = stores.for_source_key(key) else {
        return Err(permanent(
            FailureCode::InvalidKey,
            format!("{key} is under no store's source prefix"),
        ));
    };
    let stored_at = layout.source_key(&stem, BOXED_SOURCE_EXTENSION);
    if stored_at != key {
        return Err(permanent(
            FailureCode::InvalidKey,
            format!(
                "{key} is not where store {} keeps the PDF of {stem} ({stored_at})",
                store.name
            ),
        ));
    }

    // The stored size is read first: `get` buffers the whole object.
    match storage.head(key).await {
        Ok(Some(meta)) if meta.size > MAX_SOURCE_BYTES => {
            return Err(permanent(
                FailureCode::PdfParseError,
                format!(
                    "{key} is {} bytes, over the {MAX_SOURCE_BYTES}-byte limit",
                    meta.size
                ),
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(storage_failure(e, format!("head({key}) failed"))),
    }
    let pdf = match storage.get(key).await {
        Ok(bytes) => bytes::Bytes::from(bytes),
        Err(e) if hs_common::storage::is_not_found(&e) => {
            return Err(permanent(
                FailureCode::SourceMissing,
                format!("source bytes missing for {key}: {e:#}"),
            ));
        }
        Err(e) => return Err(storage_failure(e, format!("get({key}) failed"))),
    };

    let sidecar = build_on_a_server(servers, key, &pdf).await?;
    let boxes_key = word_boxes::boxes_key(layout, &stem);
    let sidecar_bytes = sidecar.len();
    storage
        .put(&boxes_key, sidecar)
        .await
        .map_err(|e| storage_failure(e, format!("put({boxes_key}) failed")))?;
    tracing::info!(
        key = %key,
        store = store.name,
        boxes_key = %boxes_key,
        sidecar_bytes,
        "word boxes stored"
    );
    Ok(boxes_key)
}

/// What one store holds towards word boxes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoxSurvey {
    /// PDFs stored where the store keeps a stem's PDF.
    pub pdfs: usize,
    /// Of those, the ones that have markdown: the PDFs a quotation can be
    /// drawn from.
    pub converted: usize,
    /// Of the converted PDFs, the ones that have a sidecar.
    pub boxed: usize,
    /// Source keys of the converted PDFs with no sidecar, sorted.
    pub missing: Vec<String>,
}

/// The stem of the PDF at `key`, when `key` is where `layout` keeps that
/// stem's PDF.
fn stored_pdf_stem<'k>(layout: &DocumentLayout, key: &'k str) -> Option<&'k str> {
    let name = key.rsplit_once('/').map_or(key, |(_, name)| name);
    let (stem, ext) = name.rsplit_once('.')?;
    (ext == BOXED_SOURCE_EXTENSION
        && hs_common::validate_stem(stem).is_ok()
        && layout.source_key(stem, BOXED_SOURCE_EXTENSION) == key)
        .then_some(stem)
}

/// The stems that have an object named `<stem>.<extension>` under `prefix`.
async fn stems_under(
    storage: &dyn Storage,
    prefix: &str,
    extension: &str,
) -> anyhow::Result<HashSet<String>> {
    let suffix = format!(".{extension}");
    let objects = storage
        .list(prefix)
        .await
        .with_context(|| format!("listing {prefix}"))?;
    Ok(objects
        .iter()
        .filter_map(|object| {
            let name = object.key.rsplit_once('/').map_or(&*object.key, |(_, n)| n);
            name.strip_suffix(&suffix).map(str::to_string)
        })
        .collect())
}

/// Count a store's PDFs, converted PDFs and sidecars, and name the
/// converted PDFs that have no sidecar.
///
/// Only a converted PDF is owed a sidecar: a quotation is located from a
/// document's markdown, so an unconverted PDF has nothing to locate, and one
/// whose conversion failed for good would be refused again on every run.
/// A listing error is an error: counting from a partial listing would queue
/// (or skip) the wrong documents.
pub async fn survey(storage: &dyn Storage, layout: &DocumentLayout) -> anyhow::Result<BoxSurvey> {
    let sources = storage
        .list(&layout.source_prefix)
        .await
        .with_context(|| format!("listing {}", layout.source_prefix))?;
    let converted = stems_under(storage, &layout.markdown_prefix, MARKDOWN_EXTENSION).await?;
    let boxed = stems_under(
        storage,
        &layout.boxes_prefix,
        word_boxes::WORD_BOXES_EXTENSION,
    )
    .await?;

    let mut survey = BoxSurvey::default();
    for object in &sources {
        let Some(stem) = stored_pdf_stem(layout, &object.key) else {
            continue;
        };
        survey.pdfs += 1;
        if !converted.contains(stem) {
            continue;
        }
        survey.converted += 1;
        if boxed.contains(stem) {
            survey.boxed += 1;
        } else {
            survey.missing.push(object.key.clone());
        }
    }
    survey.missing.sort();
    Ok(survey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_watch::tests::{storage, stores, FakeBus};
    use hs_common::stores::{ACADEMIC, PERSONAL};

    fn requested(bus: &FakeBus) -> Vec<(String, serde_json::Value)> {
        bus.published.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_request_is_one_scribe_boxes_event_naming_the_pdf() {
        let bus = FakeBus::default();
        request_boxes(&bus, "papers/sm/smith2020.pdf")
            .await
            .unwrap();
        assert_eq!(
            requested(&bus),
            [(
                "scribe.boxes".to_string(),
                serde_json::json!({ "key": "papers/sm/smith2020.pdf" })
            )]
        );
        // What was published is what the consumer reads.
        let event: BoxesEvent = serde_json::from_value(requested(&bus)[0].1.clone()).unwrap();
        assert_eq!(event.key(), "papers/sm/smith2020.pdf");
    }

    #[tokio::test]
    async fn a_failed_request_is_transient() {
        let bus = FakeBus::default();
        bus.fail_next_publishes.store(1, Ordering::SeqCst);
        let err = request_boxes(&bus, "papers/sm/smith2020.pdf")
            .await
            .unwrap_err();
        assert!(matches!(err, HandlerError::Transient(_)), "{err}");
    }

    #[tokio::test]
    async fn a_pdf_outside_every_store_is_still_requested_so_the_consumer_can_refuse_it() {
        let (_d, st) = storage();
        let bus = FakeBus::default();
        request_missing_boxes(&bus, &st, &stores(), "elsewhere/ab/abc.pdf")
            .await
            .unwrap();
        assert_eq!(requested(&bus).len(), 1);
    }

    #[tokio::test]
    async fn the_survey_counts_stored_pdfs_and_names_the_converted_ones_without_a_sidecar() {
        let (_d, st) = storage();
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let pdf = |stem: &str| layout.source_key(stem, "pdf");
        for stem in ["alpha", "bravo", "charlie", "delta"] {
            st.put(&pdf(stem), b"%PDF-".to_vec()).await.unwrap();
        }
        // alpha: converted and boxed. bravo, charlie: converted only.
        // delta: never converted.
        for stem in ["alpha", "bravo", "charlie"] {
            st.put(&layout.markdown_key(stem), b"# text".to_vec())
                .await
                .unwrap();
        }
        st.put(&word_boxes::boxes_key(layout, "alpha"), b"gz".to_vec())
            .await
            .unwrap();
        // Not the store's PDFs: an inbox drop with a converted stem, an
        // HTML source, and a sidecar's own name must not be read as a stem.
        st.put("papers/manually_downloaded/bravo.pdf", b"%PDF-".to_vec())
            .await
            .unwrap();
        st.put(&layout.source_key("echo", "html"), b"<html>".to_vec())
            .await
            .unwrap();
        st.put(&layout.markdown_key("echo"), b"# text".to_vec())
            .await
            .unwrap();

        let found = survey(&st, layout).await.unwrap();
        assert_eq!(
            found,
            BoxSurvey {
                pdfs: 4,
                converted: 3,
                boxed: 1,
                missing: vec![pdf("bravo"), pdf("charlie")],
            }
        );
    }

    #[tokio::test]
    async fn a_nested_store_is_surveyed_by_its_own_prefixes() {
        let (_d, st) = storage();
        let stores = stores();
        let (_, layout) = stores.documents(PERSONAL).unwrap();
        st.put(&layout.source_key("notes", "pdf"), b"%PDF-".to_vec())
            .await
            .unwrap();
        st.put(&layout.markdown_key("notes"), b"# text".to_vec())
            .await
            .unwrap();
        // An academic PDF with the same stem belongs to another store.
        let (_, academic) = stores.documents(ACADEMIC).unwrap();
        st.put(&academic.source_key("notes", "pdf"), b"%PDF-".to_vec())
            .await
            .unwrap();

        let found = survey(&st, layout).await.unwrap();
        assert_eq!(found.pdfs, 1);
        assert_eq!(found.missing, [layout.source_key("notes", "pdf")]);

        st.put(&word_boxes::boxes_key(layout, "notes"), b"gz".to_vec())
            .await
            .unwrap();
        let found = survey(&st, layout).await.unwrap();
        assert_eq!((found.boxed, found.missing.len()), (1, 0));
    }

    fn unreachable_servers() -> BoxServers {
        BoxServers::new(vec![ScribeClient::new("http://127.0.0.1:1").unwrap()]).unwrap()
    }

    fn outcome(result: &Result<String, HandlerError>) -> (&'static str, Option<FailureCode>) {
        match result {
            Ok(_) => ("ok", None),
            Err(HandlerError::Permanent(e)) => ("permanent", failure_code(e)),
            Err(HandlerError::Transient(e)) => ("transient", failure_code(e)),
        }
    }

    #[tokio::test]
    async fn requests_that_name_no_stored_pdf_are_refused_for_good_before_any_server_is_asked() {
        let (_d, st) = storage();
        let stores = stores();
        let servers = unreachable_servers();
        st.put("papers/manually_downloaded/drop.pdf", b"%PDF-".to_vec())
            .await
            .unwrap();
        for (key, code) in [
            ("papers/pa/page.html", FailureCode::UnsupportedExtension),
            ("elsewhere/ab/abc.pdf", FailureCode::InvalidKey),
            (
                "papers/manually_downloaded/drop.pdf",
                FailureCode::InvalidKey,
            ),
            ("../../etc/passwd.pdf", FailureCode::InvalidKey),
            ("papers/go/gone.pdf", FailureCode::SourceMissing),
        ] {
            let event = BoxesEvent { key: key.into() };
            let result = build_and_store(&st, &stores, &servers, &event).await;
            assert_eq!(outcome(&result), ("permanent", Some(code)), "{key}");
        }
    }

    #[tokio::test]
    async fn a_pdf_no_server_can_be_reached_for_is_retried_and_nothing_is_stored() {
        let (_d, st) = storage();
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let key = layout.source_key("paper", "pdf");
        st.put(&key, b"%PDF-1.4".to_vec()).await.unwrap();
        let result = build_and_store(
            &st,
            &stores,
            &unreachable_servers(),
            &BoxesEvent { key: key.clone() },
        )
        .await;
        assert_eq!(outcome(&result), ("transient", None));
        let shown = format!("{}", result.unwrap_err());
        assert!(shown.contains("http://127.0.0.1:1"), "{shown}");
        assert!(!st
            .exists(&word_boxes::boxes_key(layout, "paper"))
            .await
            .unwrap());
    }

    #[test]
    fn servers_take_turns_and_each_build_sees_every_server_once() {
        let urls = ["http://a.invalid", "http://b.invalid", "http://c.invalid"];
        let servers = BoxServers::new(
            urls.iter()
                .map(|url| ScribeClient::new(url).unwrap())
                .collect(),
        )
        .unwrap();
        assert_eq!(servers.count(), 3);
        let turn = |servers: &BoxServers| -> Vec<String> {
            servers.in_turn().map(|c| c.url().to_string()).collect()
        };
        assert_eq!(turn(&servers), urls);
        assert_eq!(
            turn(&servers),
            ["http://b.invalid", "http://c.invalid", "http://a.invalid"]
        );
        assert!(BoxServers::new(Vec::new()).is_err());
    }

    #[test]
    fn a_reply_is_accepted_only_as_the_sidecar_of_the_pdf_that_was_sent() {
        let pdf = b"%PDF-1.4 the document".to_vec();
        let mut page =
            word_boxes::PageWords::new(1, word_boxes::WordOrigin::TextLayer, 612.0, 792.0);
        page.push("word".into(), [100, 100, 900, 300]);
        let sidecar = |source: &[u8]| {
            word_boxes::encode(&word_boxes::WordBoxes {
                version: word_boxes::WORD_BOXES_VERSION,
                source_sha256: word_boxes::source_sha256(source),
                pages: vec![page.clone()],
            })
            .unwrap()
        };
        check_sidecar(&pdf, &sidecar(&pdf)).unwrap();
        let other = check_sidecar(&pdf, &sidecar(b"another document")).unwrap_err();
        assert!(format!("{other:#}").contains("names source"), "{other:#}");
        let garbage = check_sidecar(&pdf, b"not gzip").unwrap_err();
        assert!(
            format!("{garbage:#}").contains("not a word-box sidecar"),
            "{garbage:#}"
        );
    }

    // ── against the real scribe server ────────────────────────────────

    #[cfg(feature = "server")]
    mod served {
        use super::*;
        use crate::config::{AppConfig, ConverterMode};
        use crate::pdf_meta::tests::{pdf_with_pages, pdf_with_text, skip_without_pdfium};
        use crate::server::{app, ServerState};
        use hs_common::auth::backend::BackendToken;
        use std::sync::Arc;
        use std::time::Duration;

        const TOKEN: &str = "box-watch-served-token-0123456789abcdef";

        /// A scribe server with no VLM behind it and no OCR models: it can
        /// build a sidecar from a text layer and nothing else.
        async fn serve() -> String {
            let config = AppConfig {
                converter: ConverterMode::Olmocr,
                olmocr_endpoint: "http://127.0.0.1:1/v1".into(),
                vlm_concurrency: 1,
                ocr_detection_model_path: "/nonexistent/detection.onnx".into(),
                ocr_recognition_model_path: "/nonexistent/recognition.onnx".into(),
                ocr_dictionary_path: "/nonexistent/dictionary.txt".into(),
                ..AppConfig::default()
            };
            let state = Arc::new(ServerState::new(config).unwrap());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let token = BackendToken::new(TOKEN).unwrap();
            tokio::spawn(async move { axum::serve(listener, app(state, token)).await });
            base
        }

        fn client(base: &str, token: &str) -> ScribeClient {
            let http = hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                hs_common::http::client_builder().build().unwrap(),
                Some(BackendToken::new(token).unwrap()),
            );
            ScribeClient::new_with_client(base, http, Duration::from_secs(30))
        }

        const WORDS: &[&str] = &["Vector", "search", "finds", "passages", "by", "meaning"];

        #[tokio::test]
        async fn a_stored_pdf_gets_its_sidecar_from_the_first_server_that_can_build_it() {
            skip_without_pdfium!("a_stored_pdf_gets_its_sidecar");
            let base = serve().await;
            let (_d, st) = storage();
            let stores = stores();
            let (_, layout) = stores.documents(PERSONAL).unwrap();
            let key = layout.source_key("notes", "pdf");
            let pdf = pdf_with_text(&[WORDS]);
            st.put(&key, pdf.clone()).await.unwrap();
            // The unreachable server is first in turn and is passed over.
            let servers = BoxServers::new(vec![
                ScribeClient::new("http://127.0.0.1:1").unwrap(),
                client(&base, TOKEN),
            ])
            .unwrap();

            let stored_at = build_and_store(&st, &stores, &servers, &BoxesEvent { key })
                .await
                .unwrap();
            assert_eq!(stored_at, word_boxes::boxes_key(layout, "notes"));
            let boxes = word_boxes::decode(&st.get(&stored_at).await.unwrap()).unwrap();
            assert_eq!(boxes.source_sha256, word_boxes::source_sha256(&pdf));
            let words: Vec<&str> = boxes.pages[0].words.iter().map(String::as_str).collect();
            assert_eq!(words, WORDS);
        }

        #[tokio::test]
        async fn the_servers_verdicts_decide_between_never_again_and_later() {
            skip_without_pdfium!("the_servers_verdicts_decide_between_never_again_and_later");
            let base = serve().await;
            let (_d, st) = storage();
            let stores = stores();
            let (_, layout) = stores.documents(ACADEMIC).unwrap();
            let servers = BoxServers::new(vec![client(&base, TOKEN)]).unwrap();
            let build = |stem: &'static str, bytes: Vec<u8>| {
                let (st, stores, servers) = (&st, &stores, &servers);
                async move {
                    let key = layout.source_key(stem, "pdf");
                    st.put(&key, bytes).await.unwrap();
                    build_and_store(st, stores, servers, &BoxesEvent { key }).await
                }
            };

            // 415: the object is a web page. 422: a PDF pdfium cannot open.
            let html = build("webpage", b"<!DOCTYPE html><html></html>".to_vec()).await;
            assert_eq!(
                outcome(&html),
                ("permanent", Some(FailureCode::UnsupportedContentTypeHtml))
            );
            let broken = build("broken", b"%PDF-1.4\nnot a document".to_vec()).await;
            assert_eq!(
                outcome(&broken),
                ("permanent", Some(FailureCode::PdfParseError))
            );
            // 500: a page with no text layer needs the OCR models, which
            // this host does not have. Another day, or another server, can.
            let scan = build("scan", pdf_with_pages(1)).await;
            assert_eq!(outcome(&scan), ("transient", None));
            let shown = format!("{}", scan.unwrap_err());
            assert!(shown.contains("detection.onnx"), "{shown}");

            for stem in ["webpage", "broken", "scan"] {
                assert!(
                    !st.exists(&word_boxes::boxes_key(layout, stem))
                        .await
                        .unwrap(),
                    "{stem}"
                );
            }
        }

        #[tokio::test]
        async fn a_rejected_token_is_reported_as_such_so_the_watcher_stops() {
            let base = serve().await;
            let (_d, st) = storage();
            let stores = stores();
            let (_, layout) = stores.documents(ACADEMIC).unwrap();
            let key = layout.source_key("paper", "pdf");
            st.put(&key, b"%PDF-1.4".to_vec()).await.unwrap();
            let servers = BoxServers::new(vec![client(
                &base,
                "another-token-entirely-0123456789abcdef",
            )])
            .unwrap();
            let err = build_and_store(&st, &stores, &servers, &BoxesEvent { key })
                .await
                .unwrap_err();
            let HandlerError::Transient(inner) = &err else {
                panic!("a refused token is not a verdict on the document: {err}");
            };
            assert!(crate::client::is_backend_unauthorized(inner), "{err}");
        }
    }
}
