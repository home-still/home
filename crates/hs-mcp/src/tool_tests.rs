//! Behavior of the tools against fake storage and a loopback distill server.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;

use crate::testkit::{seed_catalog, seed_markdown, server, FakeDistill, FaultyStorage};
use crate::{DistillReconcileParams, DistillReindexParams};

mod reindex {
    use super::*;

    fn reindex(stem: &str) -> Parameters<DistillReindexParams> {
        Parameters(DistillReindexParams {
            stem: crate::stem::Stem::parse(stem).unwrap(),
        })
    }

    /// RA-9: the old tool deleted the document's vectors before it had read
    /// the catalog or verified the markdown, so a storage blip left the
    /// document with no vectors and nothing re-indexed.
    #[tokio::test]
    async fn a_storage_outage_is_an_error_and_no_vector_is_deleted() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "paper-a",
            "# A\n\nBody text long enough to index.",
        )
        .await;
        seed_catalog(&*storage, "paper-a").await;
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.distill_reindex(reindex("paper-a")).await.unwrap_err();

        assert!(
            err.contains("simulated storage outage"),
            "the cause must reach the caller: {err}"
        );
        assert_eq!(
            distill.requests(),
            vec![],
            "no request (certainly no DELETE) may reach the vector store"
        );
        assert!(storage.recorded_deletes().is_empty());
    }

    #[tokio::test]
    async fn missing_markdown_is_an_error_and_the_vectors_stay() {
        let storage = FaultyStorage::new();
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        let err = mcp
            .distill_reindex(reindex("never-converted"))
            .await
            .unwrap_err();

        assert!(err.contains("Markdown not found"), "{err}");
        assert_eq!(distill.count("DELETE"), 0);
        assert_eq!(distill.count("POST"), 0);
    }

    #[tokio::test]
    async fn reindex_replaces_in_place_with_one_index_call_and_no_delete() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "paper-a",
            "# A\n\nBody text long enough to index.",
        )
        .await;
        seed_catalog(&*storage, "paper-a").await;
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        let out = mcp.distill_reindex(reindex("paper-a")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["chunks_indexed"], 3);
        assert_eq!(out["has_catalog"], true);
        assert_eq!(distill.count("POST"), 1);
        assert_eq!(distill.count("DELETE"), 0);
        let entry = hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", "paper-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.embedding.map(|e| e.chunks_indexed),
            Some(3),
            "the catalog must carry the new embedding stamp"
        );
    }
}

mod reconcile {
    use super::*;

    fn reconcile() -> DistillReconcileParams {
        DistillReconcileParams {
            dry_run: true,
            limit: None,
        }
    }

    /// RA-32: `exists().unwrap_or(false)` reported every document as an
    /// orphan during a storage outage — a purge list built from a blip.
    #[tokio::test]
    async fn a_storage_outage_is_an_error_not_a_list_of_orphans() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "present",
            "# P\n\nBody text long enough to index.",
        )
        .await;
        let distill = FakeDistill::start(vec!["present".into(), "gone".into()]).await;
        let mcp = server(storage.clone(), Some(&distill));

        // Catalog reads keep working; only the markdown existence probe
        // fails, as when one S3 prefix times out.
        *storage.only_keys_containing.lock().unwrap() = Some("markdown/".into());
        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.reconcile_orphans(reconcile()).await.unwrap_err();

        assert!(err.contains("simulated storage outage"), "{err}");
    }

    #[tokio::test]
    async fn only_ids_without_markdown_are_orphans() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "present",
            "# P\n\nBody text long enough to index.",
        )
        .await;
        let distill = FakeDistill::start(vec!["present".into(), "gone".into()]).await;
        let mcp = server(storage, Some(&distill));

        let out = mcp.reconcile_orphans(reconcile()).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["orphans"], serde_json::json!(["gone"]));
        assert_eq!(out["scanned_doc_ids"], 2);
    }

    /// The description promises a notice when `dry_run=false` is ignored, and
    /// the tool used to discard the argument silently: an agent that asked to
    /// apply read a report as a success. The response now says so, and says
    /// `false` when nobody asked.
    #[tokio::test]
    async fn an_apply_request_is_reported_as_ignored() {
        let storage = FaultyStorage::new();
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage, Some(&distill));

        let asked = mcp
            .reconcile_orphans(DistillReconcileParams {
                dry_run: false,
                limit: None,
            })
            .await
            .unwrap();
        let asked: serde_json::Value = serde_json::from_str(&asked).unwrap();
        assert_eq!(
            asked["requested_apply_ignored"], true,
            "a caller that asked to apply must not read a dry-run report as a success: {asked}"
        );
        assert_eq!(
            asked["dry_run"], true,
            "the report is still a dry run: {asked}"
        );

        let not_asked = mcp.reconcile_orphans(reconcile()).await.unwrap();
        let not_asked: serde_json::Value = serde_json::from_str(&not_asked).unwrap();
        assert_eq!(
            not_asked["requested_apply_ignored"], false,
            "the field reports what the caller asked for, not a constant: {not_asked}"
        );
    }

    /// The tool asks the server for the academic store's whole list; the old
    /// 100 000 default made a larger store fail outright.
    #[tokio::test]
    async fn the_whole_store_is_requested() {
        let storage: Arc<FaultyStorage> = FaultyStorage::new();
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage, Some(&distill));

        mcp.reconcile_orphans(reconcile()).await.unwrap();

        let docs_requests: Vec<_> = distill
            .requests()
            .into_iter()
            .filter(|s| s.path == "/docs")
            .collect();
        assert_eq!(docs_requests.len(), 1);
        assert_eq!(
            docs_requests[0].query,
            format!(
                "store={}&limit={}",
                hs_common::stores::ACADEMIC,
                hs_distill::client::MAX_DOC_LIST_LIMIT
            )
        );
    }
}

mod convert {
    use super::*;
    use crate::stem::Stem;
    use crate::testkit::{server_with_bus, RecordingBus};
    use hs_common::storage::Storage;

    const ARTICLE: &str = "<html><body><article><h1>A study of things</h1>\
        <p>This paragraph is long enough to clear the indexable floor of the pipeline.</p>\
        <p>A second paragraph keeps the converter from producing a stub document.</p>\
        </article></body></html>";

    fn stem(s: &str) -> Stem {
        Stem::parse(s).unwrap()
    }

    async fn put_source(storage: &FaultyStorage, stem: &str, ext: &str, bytes: &[u8]) -> String {
        use hs_common::storage::Storage;
        let key = format!("papers/{}", hs_common::sharded_key(stem, ext));
        storage.put(&key, bytes.to_vec()).await.unwrap();
        key
    }

    /// RA-31: `if let Ok(pdf) = storage.get(..)` read an S3 outage as "no
    /// PDF" and converted the HTML twin instead, stamping the catalog with a
    /// worse document than the PDF the corpus holds.
    #[tokio::test]
    async fn a_storage_outage_never_sends_the_stem_to_another_converter() {
        let storage = FaultyStorage::new();
        put_source(&storage, "paper", "pdf", b"%PDF-1.4 stand-in").await;
        put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        *storage.only_keys_containing.lock().unwrap() = Some(".pdf".into());
        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.convert_source(&stem("paper")).await.unwrap_err();

        assert!(err.contains("simulated storage outage"), "{err}");
        assert!(
            !storage
                .exists(&hs_common::markdown::markdown_storage_key("paper"))
                .await
                .unwrap(),
            "nothing may be converted"
        );
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_source_names_every_key_that_was_tried() {
        let storage = FaultyStorage::new();
        let mcp = server_with_bus(storage, Arc::new(RecordingBus::default()));

        let err = mcp.convert_source(&stem("ghost")).await.unwrap_err();

        for ext in ["pdf", "html", "epub"] {
            assert!(err.contains(&format!("ghost.{ext}")), "{err}");
        }
    }

    /// HTML goes through the watcher's function: markdown stored, catalog
    /// stamped by the html parser, `scribe.completed` announced.
    #[tokio::test]
    async fn an_html_source_is_converted_by_the_shared_path() {
        let storage = FaultyStorage::new();
        let source_key = put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = mcp.convert_source(&stem("paper")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["server"], "html-parser");
        assert_eq!(out["already_converted"], false);
        let md = hs_common::markdown::read_markdown_via(&*storage, "markdown", "paper")
            .await
            .unwrap()
            .expect("markdown stored");
        assert!(md.contains("A study of things"), "{md}");
        let published = bus.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "scribe.completed");
        assert_eq!(published[0].1["source_key"], source_key);
    }

    /// A paywall page is refused, not converted into markdown, and the
    /// refusal is the same one the watcher records.
    #[tokio::test]
    async fn a_paywall_page_is_refused_and_nothing_is_stored() {
        let storage = FaultyStorage::new();
        put_source(
            &storage,
            "wall",
            "html",
            b"<html><body><p>Access denied. Sign in to read this article.</p></body></html>",
        )
        .await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let err = mcp.convert_source(&stem("wall")).await.unwrap_err();

        assert!(err.to_lowercase().contains("paywall"), "{err}");
        assert!(!storage
            .exists(&hs_common::markdown::markdown_storage_key("wall"))
            .await
            .unwrap());
        assert!(bus.published.lock().unwrap().is_empty());
        let entry = hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", "wall")
            .await
            .unwrap();
        assert!(
            entry.is_none_or(|e| e.conversion.is_none()),
            "a refused document must not carry a conversion stamp"
        );
    }

    #[tokio::test]
    async fn existing_markdown_is_announced_not_converted_again() {
        let storage = FaultyStorage::new();
        put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        seed_markdown(
            &*storage,
            "paper",
            "# Already here\n\nBody text long enough to index.",
        )
        .await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = mcp.convert_source(&stem("paper")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["already_converted"], true);
        let md = hs_common::markdown::read_markdown_via(&*storage, "markdown", "paper")
            .await
            .unwrap()
            .unwrap();
        assert!(
            md.contains("Already here"),
            "the stored markdown must be untouched"
        );
        assert_eq!(bus.published.lock().unwrap().len(), 1);
    }

    /// A PDF that was converted before word boxes existed (or whose request
    /// was lost) gets its sidecar asked for along with the announcement.
    #[tokio::test]
    async fn an_already_converted_pdf_without_word_boxes_has_them_requested() {
        let storage = FaultyStorage::new();
        let source_key = put_source(&storage, "scan", "pdf", b"%PDF-1.4").await;
        seed_markdown(&*storage, "scan", "# Already here\n\nBody text.").await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        mcp.convert_source(&stem("scan")).await.unwrap();

        let published = bus.published.lock().unwrap().clone();
        let subjects: Vec<&str> = published.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(subjects, ["scribe.completed", "scribe.boxes"]);
        assert_eq!(published[1].1, serde_json::json!({ "key": source_key }));
    }

    /// The queue tool's whole point: the conversion is announced, not run.
    /// A caller that gets `queued: true` back must be able to see the event
    /// the watcher will consume, with the keys the watcher reads.
    #[tokio::test]
    async fn queueing_publishes_the_ingest_event_and_converts_nothing() {
        let storage = FaultyStorage::new();
        let source_key = put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = mcp
            .scribe_queue(Parameters(crate::ScribeQueueParams {
                stem: stem("paper"),
            }))
            .await
            .unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["queued"], true);
        assert_eq!(out["stem"], "paper");
        assert_eq!(out["source_key"], source_key);

        let published = bus.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].0,
            hs_common::event_bus::specs::PAPERS_INGESTED.subject
        );
        assert_eq!(published[0].1["key"], source_key);
        assert_eq!(published[0].1["source"], "mcp:scribe_queue");

        // Queueing is an announcement, not a conversion.
        assert!(
            !storage
                .exists(&hs_common::markdown::markdown_storage_key("paper"))
                .await
                .unwrap(),
            "the tool must not convert anything itself"
        );
    }

    /// A queued stem with no stored source would fail in the watcher with no
    /// one listening, so the tool refuses it at the door.
    #[tokio::test]
    async fn queueing_a_stem_with_no_source_publishes_nothing() {
        let storage = FaultyStorage::new();
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage, bus.clone());

        let err = mcp
            .scribe_queue(Parameters(crate::ScribeQueueParams {
                stem: stem("ghost"),
            }))
            .await
            .unwrap_err();

        for ext in ["pdf", "html", "epub"] {
            assert!(err.contains(&format!("ghost.{ext}")), "{err}");
        }
        assert!(bus.published.lock().unwrap().is_empty());
    }
}

mod cite {
    use std::sync::Mutex;

    use base64::Engine as _;

    use super::*;
    use crate::stem::Stem;
    use crate::testkit::{server_with_docs_service, server_with_links, RecordingBus};
    use crate::tools::cite::CiteParams;
    use crate::HomeStillMcp;
    use hs_common::catalog::{AuthorEntry, CatalogEntry, ConversionMeta};
    use hs_common::docs::SHORT_LINK_PREFIX;
    use hs_common::markdown::PAGE_SEPARATOR;
    use hs_common::storage::Storage;
    use hs_common::stores::{ABSTRACTS, ACADEMIC, MEMORY, PERSONAL, REFERENCES};
    use hs_common::word_boxes::{self, PageWords, WordBoxes, WordOrigin, WORD_BOXES_VERSION};

    const STEM: &str = "10.1234_abcd";
    const PAGE_ONE: &str =
        "# A study of rivers\n\nRivers carry sediment from the mountains to the sea.";
    const PAGE_TWO: &str = "The delta grows where the current slows and the silt settles out.";
    /// Words 4 to 11 of page two.
    const QUOTE: &str = "the current slows and the silt settles out";
    /// The markdown line page two's text is on.
    const QUOTE_LINE: u32 = 7;
    /// [`QUOTE`] with one word the document does not say.
    const MISQUOTE: &str = "the current slows and the mud settles out";
    const ABSTRACT: &str = "We measure how deltas grow when rivers slow near the coast.";

    // The sidecar's pages: words on a grid, six to a line.
    const WORDS_PER_LINE: usize = 6;
    const PAGE_WIDTH_PT: f32 = 612.0;
    const PAGE_HEIGHT_PT: f32 = 792.0;
    const FIRST_LINE_TOP: u16 = 1_000;
    const LINE_PITCH: u16 = 300;
    const LINE_HEIGHT: u16 = 200;
    const FIRST_WORD_LEFT: u16 = 1_000;
    const WORD_PITCH: u16 = 800;
    const WORD_WIDTH: u16 = 700;

    /// The boxes of the quote's words on that grid, one per line: the last
    /// two words of page two's first line, and the whole of its second.
    const QUOTE_LINES: [[u64; 4]; 2] = [[4_200, 1_000, 5_700, 1_200], [1_000, 1_300, 5_700, 1_500]];

    /// The right edge of the word before the quote, which no rectangle may
    /// reach.
    const WORD_BEFORE_QUOTE_RIGHT: u64 = 4_100;

    /// Whether `rect` (`[x0, y0, x1, y1]`) holds the whole of `words`. A
    /// line's rectangle is padded and the gap between lines closed, which
    /// `hs_common::quote` tests; here the question is the reader's: is
    /// every quoted word inside a box?
    fn covers(rect: &serde_json::Value, words: [u64; 4]) -> bool {
        let edge = |i: usize| rect[i].as_u64();
        matches!(
            (edge(0), edge(1), edge(2), edge(3)),
            (Some(x0), Some(y0), Some(x1), Some(y1))
                if x0 <= words[0] && y0 <= words[1] && x1 >= words[2] && y1 >= words[3]
        )
    }

    /// The highlight a link to the quote carries: page 2, words 4 to 11.
    const QUOTE_HIGHLIGHT: &str = "&page=2&hl=2.4-12&sig=";

    fn markdown() -> String {
        format!("{PAGE_ONE}{PAGE_SEPARATOR}{PAGE_TWO}")
    }

    /// A page whose words are `text`'s, laid out on the grid.
    fn page(number: u32, text: &str) -> PageWords {
        let mut page = PageWords::new(number, WordOrigin::TextLayer, PAGE_WIDTH_PT, PAGE_HEIGHT_PT);
        for (index, word) in text.split_whitespace().enumerate() {
            let top = FIRST_LINE_TOP + LINE_PITCH * u16::try_from(index / WORDS_PER_LINE).unwrap();
            let left =
                FIRST_WORD_LEFT + WORD_PITCH * u16::try_from(index % WORDS_PER_LINE).unwrap();
            page.push(
                word.to_string(),
                [left, top, left + WORD_WIDTH, top + LINE_HEIGHT],
            );
        }
        page
    }

    fn sidecar(pages: Vec<PageWords>) -> WordBoxes {
        WordBoxes {
            version: WORD_BOXES_VERSION,
            source_sha256: "0".repeat(64),
            pages,
        }
    }

    /// The document's own pages, as its word boxes.
    fn own_boxes() -> WordBoxes {
        sidecar(vec![page(1, PAGE_ONE), page(2, PAGE_TWO)])
    }

    /// The document's catalog entry, as a conversion leaves it.
    fn card() -> CatalogEntry {
        CatalogEntry {
            title: Some("A study of rivers".into()),
            authors: vec![
                AuthorEntry {
                    name: "Ada Lovelace".into(),
                },
                AuthorEntry {
                    name: "Grace Hopper".into(),
                },
            ],
            doi: Some("10.1234/abcd".into()),
            publication_date: Some("2019-05-02".into()),
            abstract_text: Some(ABSTRACT.into()),
            conversion: Some(ConversionMeta {
                server: "test".into(),
                duration_secs: 0.0,
                total_pages: 2,
                converted_at: "2026-01-01T00:00:00Z".into(),
                pages: hs_common::catalog::compute_page_offsets(&markdown()),
                converted_by: None,
                attempts_log: Vec::new(),
            }),
            ..Default::default()
        }
    }

    fn linked(storage: &Arc<FaultyStorage>) -> HomeStillMcp {
        server_with_links(storage.clone(), Arc::new(RecordingBus::default()))
    }

    /// Put the document into the academic store: a source with `extension`,
    /// its converted text and `card`.
    async fn seed_with(
        mcp: &HomeStillMcp,
        storage: &dyn Storage,
        extension: &str,
        card: &CatalogEntry,
    ) {
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        storage
            .put(&layout.source_key(STEM, extension), b"the source".to_vec())
            .await
            .unwrap();
        storage
            .put(&layout.markdown_key(STEM), markdown().into_bytes())
            .await
            .unwrap();
        hs_common::catalog::write_catalog_entry_via(storage, &layout.catalog_prefix, STEM, card)
            .await
            .unwrap();
    }

    async fn seed(mcp: &HomeStillMcp, storage: &dyn Storage, extension: &str) {
        seed_with(mcp, storage, extension, &card()).await;
    }

    async fn seed_boxes(mcp: &HomeStillMcp, storage: &dyn Storage, boxes: &WordBoxes) {
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        storage
            .put(
                &word_boxes::boxes_key(layout, STEM),
                word_boxes::encode(boxes).unwrap(),
            )
            .await
            .unwrap();
    }

    fn ask(quote: &str) -> CiteParams {
        CiteParams {
            store: None,
            doc_id: Stem::parse(STEM).unwrap(),
            quote: quote.to_string(),
            near_line: None,
            image: false,
            short: None,
        }
    }

    fn ask_in(store: &str, quote: &str) -> CiteParams {
        CiteParams {
            store: Some(store.to_string()),
            ..ask(quote)
        }
    }

    async fn cite(mcp: &HomeStillMcp, params: CiteParams) -> serde_json::Value {
        serde_json::to_value(mcp.cite_quote(params).await.unwrap().citation).unwrap()
    }

    async fn short_links(storage: &FaultyStorage) -> usize {
        storage.list(SHORT_LINK_PREFIX).await.unwrap().len()
    }

    #[tokio::test]
    async fn a_located_quote_comes_back_with_its_page_its_rectangles_and_its_links() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        seed_boxes(&mcp, &*storage, &own_boxes()).await;

        let out = cite(&mcp, ask(QUOTE)).await;

        assert_eq!(out["store"], ACADEMIC);
        assert_eq!(out["doc_id"], STEM);
        assert_eq!(out["title"], "A study of rivers");
        assert_eq!(
            out["authors"],
            serde_json::json!(["Ada Lovelace", "Grace Hopper"])
        );
        assert_eq!(out["year"], 2019);
        assert_eq!(out["doi"], "10.1234/abcd");
        assert_eq!(
            out["quote"],
            serde_json::json!({
                "text": QUOTE,
                "exact": true,
                "found_in": "document",
                "page": 2,
                "line_start": QUOTE_LINE,
                "line_end": QUOTE_LINE,
            })
        );
        let boxes = &out["boxes"];
        assert_eq!(boxes["status"], "located");
        assert_eq!(boxes["origin"], "text_layer");
        assert_eq!(boxes["matched_ratio"], 1.0);
        let pages = boxes["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 1, "{boxes}");
        assert_eq!(pages[0]["page"], 2);
        let rects = pages[0]["rects"].as_array().unwrap();
        assert_eq!(rects.len(), QUOTE_LINES.len(), "{boxes}");
        for (rect, words) in rects.iter().zip(QUOTE_LINES) {
            assert!(covers(rect, words), "{rect} does not hold {words:?}");
        }
        assert!(
            rects[0][0].as_u64().unwrap() > WORD_BEFORE_QUOTE_RIGHT,
            "a box takes in the word before the quote: {boxes}"
        );

        let links = &out["links"];
        let viewer = links["viewer"]["lan"].as_str().unwrap();
        assert!(
            viewer.starts_with(&format!("http://host:7446/docs/v1/{STEM}/viewer?")),
            "{viewer}"
        );
        assert!(viewer.contains("&col=academic&"), "{viewer}");
        assert!(viewer.contains(QUOTE_HIGHLIGHT), "{viewer}");
        let page = links["page"]["remote"].as_str().unwrap();
        assert!(
            page.starts_with(&format!("https://public.example/docs/v1/{STEM}/page?")),
            "{page}"
        );
        assert!(page.contains(QUOTE_HIGHLIGHT), "{page}");
        let source = links["source"]["lan"].as_str().unwrap();
        assert!(source.contains("/source?"), "{source}");
        assert!(source.ends_with("#page=2"), "{source}");
        let text = links["markdown"]["lan"].as_str().unwrap();
        assert!(text.contains("/markdown?"), "{text}");
        assert!(text.contains("#:~:text="), "{text}");
        assert!(links["short"].is_null(), "{links}");
        assert!(links["original_url"].is_null(), "{links}");
    }

    /// The passage is returned as the document has it, and `exact` says
    /// when that is not how it was typed.
    #[tokio::test]
    async fn a_quote_typed_differently_comes_back_in_the_documents_words() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let out = cite(&mcp, ask("The  CURRENT slows,\nand the silt settles out")).await;

        assert_eq!(out["quote"]["exact"], false);
        assert_eq!(out["quote"]["text"], QUOTE);
    }

    /// A PDF whose word boxes are not built yet is still cited: the page
    /// comes from the offsets the conversion recorded, and the links that
    /// need a highlight are left out rather than minted without one.
    #[tokio::test]
    async fn a_pdf_without_word_boxes_is_pending_and_takes_its_page_from_the_catalog() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let out = cite(&mcp, ask(QUOTE)).await;

        assert_eq!(out["boxes"], serde_json::json!({ "status": "pending" }));
        assert_eq!(out["quote"]["page"], 2);
        let links = out["links"].as_object().unwrap();
        assert!(!links.contains_key("viewer"), "{links:?}");
        assert!(!links.contains_key("page"), "{links:?}");
        assert!(
            links["source"]["lan"]
                .as_str()
                .unwrap()
                .ends_with("#page=2"),
            "{links:?}"
        );
    }

    #[tokio::test]
    async fn word_boxes_that_do_not_hold_the_quote_leave_it_not_located() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        let other = sidecar(vec![
            page(1, "zebra quartz xylophone jumped over wizard boxes"),
            page(2, "nine vexed sphinxes judge my quick waltz"),
        ]);
        seed_boxes(&mcp, &*storage, &other).await;

        let out = cite(&mcp, ask(QUOTE)).await;

        assert_eq!(out["boxes"], serde_json::json!({ "status": "not_located" }));
        assert_eq!(out["quote"]["line_start"], QUOTE_LINE);
        assert!(out["links"].get("viewer").is_none(), "{out}");
    }

    /// A sidecar that cannot be read is a fault, not "no boxes yet": the
    /// caller would wait for a catch-up that has nothing to do.
    #[tokio::test]
    async fn word_boxes_that_cannot_be_read_are_an_error() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        storage
            .put(
                &word_boxes::boxes_key(layout, STEM),
                b"not a sidecar".to_vec(),
            )
            .await
            .unwrap();

        let err = mcp.cite_quote(ask(QUOTE)).await.unwrap_err();

        assert!(err.contains("word boxes"), "{err}");
    }

    /// An HTML source has no pages to box anything on; its card's address
    /// is the page the document was captured from, and the link to it
    /// selects the passage.
    #[tokio::test]
    async fn an_html_document_has_no_boxes_and_links_to_where_it_was_captured() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        let card = CatalogEntry {
            source_url: Some("https://example.org/rivers#top".into()),
            ..card()
        };
        seed_with(&mcp, &*storage, "html", &card).await;

        let out = cite(&mcp, ask(QUOTE)).await;

        assert_eq!(
            out["boxes"],
            serde_json::json!({ "status": "not_applicable" })
        );
        assert!(out["quote"]["page"].is_null(), "{out}");
        assert_eq!(out["source_url"], "https://example.org/rivers#top");
        let original = out["links"]["original_url"].as_str().unwrap();
        assert!(
            original.starts_with("https://example.org/rivers#:~:text="),
            "{original}"
        );
    }

    /// The reason the tool exists: words the document does not say are
    /// refused, the caller is shown what it does say, and nothing is stored.
    #[tokio::test]
    async fn a_quote_the_document_does_not_say_is_a_tool_error_naming_the_closest_passage() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        seed_boxes(&mcp, &*storage, &own_boxes()).await;

        let result = mcp
            .cite(Parameters(CiteParams {
                short: Some("source".into()),
                ..ask(MISQUOTE)
            }))
            .await;

        let result = serde_json::to_value(result.unwrap()).unwrap();
        assert_eq!(result["isError"], true, "{result}");
        let said = result["content"][0]["text"].as_str().unwrap();
        assert!(said.contains("does not contain the quote"), "{said}");
        assert!(said.contains("the silt settles out"), "{said}");
        assert_eq!(short_links(&storage).await, 0);
    }

    #[tokio::test]
    async fn an_unknown_store_the_memory_store_and_an_unknown_document_are_refused() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let err = mcp.cite_quote(ask_in("nope", QUOTE)).await.unwrap_err();
        assert!(err.contains("no store named \"nope\""), "{err}");

        let err = mcp.cite_quote(ask_in(MEMORY, QUOTE)).await.unwrap_err();
        assert!(err.contains("holds no documents"), "{err}");

        let err = mcp
            .cite_quote(CiteParams {
                doc_id: Stem::parse("ghost").unwrap(),
                ..ask(QUOTE)
            })
            .await
            .unwrap_err();
        assert!(err.contains("has no document \"ghost\""), "{err}");
    }

    /// The same answer for "no such document" during an outage would send
    /// the caller looking for a document that is there.
    #[tokio::test]
    async fn a_storage_outage_is_not_read_as_a_missing_document() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.cite_quote(ask(QUOTE)).await.unwrap_err();

        assert!(err.contains("simulated storage outage"), "{err}");
    }

    /// A hit from `abstracts` is the catalog entry's abstract, not a line
    /// of the document: that is the text the quote is checked against, and
    /// the links are the parent store's.
    #[tokio::test]
    async fn an_abstract_is_checked_against_the_catalog_entry() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        seed_boxes(&mcp, &*storage, &own_boxes()).await;

        let out = cite(&mcp, ask_in(ABSTRACTS, "deltas grow when rivers slow")).await;

        assert_eq!(out["store"], ABSTRACTS);
        assert_eq!(
            out["quote"],
            serde_json::json!({
                "text": "deltas grow when rivers slow",
                "exact": true,
                "found_in": "abstract",
                "page": null,
                "line_start": null,
                "line_end": null,
            })
        );
        assert_eq!(
            out["boxes"],
            serde_json::json!({ "status": "not_applicable" })
        );
        let source = out["links"]["source"]["lan"].as_str().unwrap();
        assert!(source.contains("&col=academic&"), "{source}");
        let text = out["links"]["markdown"]["lan"].as_str().unwrap();
        assert!(!text.contains('#'), "{text}");

        // The document's own words are not the abstract's.
        let err = mcp.cite_quote(ask_in(ABSTRACTS, QUOTE)).await.unwrap_err();
        assert!(err.contains("the abstract of"), "{err}");

        let err = mcp
            .cite_quote(CiteParams {
                near_line: Some(QUOTE_LINE),
                ..ask_in(ABSTRACTS, "deltas grow when rivers slow")
            })
            .await
            .unwrap_err();
        assert!(err.contains("near_line"), "{err}");
    }

    #[tokio::test]
    async fn an_abstract_the_catalog_entry_does_not_hold_points_at_the_document() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        let card = CatalogEntry {
            abstract_text: None,
            ..card()
        };
        seed_with(&mcp, &*storage, "pdf", &card).await;

        let err = mcp
            .cite_quote(ask_in(ABSTRACTS, "deltas grow when rivers slow"))
            .await
            .unwrap_err();

        assert!(err.contains("holds no abstract"), "{err}");
        assert!(err.contains("\"academic\""), "{err}");
    }

    /// A reference list is part of its document: the quote is checked
    /// against the document and the links open it.
    #[tokio::test]
    async fn a_reference_is_cited_from_the_document_it_belongs_to() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let out = cite(&mcp, ask_in(REFERENCES, QUOTE)).await;

        assert_eq!(out["store"], REFERENCES);
        assert_eq!(out["quote"]["found_in"], "document");
        let source = out["links"]["source"]["lan"].as_str().unwrap();
        assert!(source.contains("&col=academic&"), "{source}");
    }

    /// The personal store keeps a document's card beside the document.
    #[tokio::test]
    async fn a_personal_document_is_cited_with_the_card_beside_it() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        let (_, layout) = mcp.stores.documents(PERSONAL).unwrap();
        let card = CatalogEntry {
            title: Some("Field notes".into()),
            ..Default::default()
        };
        for (key, bytes) in [
            (layout.source_key(STEM, "txt"), b"the source".to_vec()),
            (layout.markdown_key(STEM), markdown().into_bytes()),
            (
                layout.source_key(STEM, "catalog.yaml"),
                serde_yaml_ng::to_string(&card).unwrap().into_bytes(),
            ),
        ] {
            storage.put(&key, bytes).await.unwrap();
        }

        let out = cite(&mcp, ask_in(PERSONAL, QUOTE)).await;

        assert_eq!(out["title"], "Field notes");
        assert_eq!(
            out["boxes"],
            serde_json::json!({ "status": "not_applicable" })
        );
        let source = out["links"]["source"]["lan"].as_str().unwrap();
        assert!(source.contains("&col=personal&"), "{source}");
    }

    /// A short link is an alias, and minting one is a write: the code has to
    /// exist in the corpus before the caller is told it works, and what it
    /// redirects to is the link the caller was also given.
    #[tokio::test]
    async fn a_short_link_is_stored_and_redirects_to_the_link_it_names() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;
        seed_boxes(&mcp, &*storage, &own_boxes()).await;

        let out = cite(
            &mcp,
            CiteParams {
                short: Some("viewer".into()),
                ..ask(QUOTE)
            },
        )
        .await;

        let short = &out["links"]["short"];
        let code = short["code"].as_str().unwrap();
        assert_eq!(short["lan"], format!("http://host:7446/s/{code}"));
        assert_eq!(short["remote"], format!("https://public.example/s/{code}"));
        let stored = hs_common::docs::ShortLinks::new(storage.clone())
            .get(code)
            .await
            .unwrap()
            .expect("the code the caller was given must resolve");
        assert_eq!(
            format!("http://host:7446{}", stored.location()),
            out["links"]["viewer"]["lan"].as_str().unwrap()
        );
    }

    /// Without `short` nothing is written: a citation must not fill the
    /// corpus with aliases.
    #[tokio::test]
    async fn a_citation_without_short_stores_nothing() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let out = cite(&mcp, ask(QUOTE)).await;

        assert!(out["links"]["short"].is_null(), "{out}");
        assert_eq!(short_links(&storage).await, 0);
    }

    /// The viewer and the page view exist only for a located passage, so a
    /// code for either has nothing to stand for.
    #[tokio::test]
    async fn a_short_link_to_a_view_the_passage_does_not_have_is_refused() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let err = mcp
            .cite_quote(CiteParams {
                short: Some("page".into()),
                ..ask(QUOTE)
            })
            .await
            .unwrap_err();
        assert!(err.contains("`pending`"), "{err}");

        let err = mcp
            .cite_quote(CiteParams {
                short: Some("pdf".into()),
                ..ask(QUOTE)
            })
            .await
            .unwrap_err();
        assert!(err.contains("unknown short link \"pdf\""), "{err}");
        assert_eq!(short_links(&storage).await, 0);
    }

    #[tokio::test]
    async fn a_host_that_mints_no_links_says_which_settings_are_missing() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);

        let err = mcp.cite_quote(ask(QUOTE)).await.unwrap_err();

        assert!(err.contains("docs.lan_base_url"), "{err}");
        assert!(err.contains("docs.remote_base_url"), "{err}");
    }

    /// A stand-in docs service: answers every page image request the same
    /// way and remembers the query of each.
    struct FakeDocs {
        url: String,
        queries: Arc<Mutex<Vec<String>>>,
    }

    impl FakeDocs {
        async fn start(status: axum::http::StatusCode, media_type: &'static str) -> Self {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let seen = queries.clone();
            let app = axum::Router::new().route(
                "/docs/v1/{stem}/page.png",
                axum::routing::get(
                    move |axum::extract::RawQuery(query): axum::extract::RawQuery| {
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(query.unwrap_or_default());
                            (
                                status,
                                [(axum::http::header::CONTENT_TYPE, media_type)],
                                PAGE_PNG.to_vec(),
                            )
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { url, queries }
        }
    }

    /// What the stand-in answers with. `cite` relays the bytes; it does not
    /// decode them.
    const PAGE_PNG: &[u8] = b"\x89PNG\r\n\x1a\nthe page, the passage boxed";

    async fn located_behind(docs: &FakeDocs) -> HomeStillMcp {
        let storage = FaultyStorage::new();
        let mcp = server_with_docs_service(storage.clone(), &docs.url);
        seed(&mcp, &*storage, "pdf").await;
        seed_boxes(&mcp, &*storage, &own_boxes()).await;
        mcp
    }

    fn ask_image() -> CiteParams {
        CiteParams {
            image: true,
            ..ask(QUOTE)
        }
    }

    /// `image: true` is the page as the docs service draws it: the request
    /// is the signed page-image link, highlight included, and the picture
    /// follows the citation in the result.
    #[tokio::test]
    async fn an_image_is_the_boxed_page_the_docs_service_draws() {
        let docs = FakeDocs::start(axum::http::StatusCode::OK, "image/png").await;
        let mcp = located_behind(&docs).await;

        let cited = mcp.cite_quote(ask_image()).await.unwrap();

        assert_eq!(cited.page_image.as_deref(), Some(PAGE_PNG));
        let queries = docs.queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 1, "{queries:?}");
        assert!(queries[0].contains(QUOTE_HIGHLIGHT), "{queries:?}");

        let content = serde_json::to_value(cited.into_contents().unwrap()).unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(content[1]["data"].as_str().unwrap())
                .unwrap(),
            PAGE_PNG
        );
    }

    /// The link is a capability: a failure to fetch it must not hand it to
    /// whoever reads the error.
    #[tokio::test]
    async fn a_page_image_the_docs_service_refuses_is_an_error_without_the_signed_link() {
        let docs = FakeDocs::start(axum::http::StatusCode::NOT_FOUND, "text/plain").await;
        let mcp = located_behind(&docs).await;

        let err = mcp.cite_quote(ask_image()).await.unwrap_err();

        assert!(err.contains("404"), "{err}");
        assert!(!err.contains("sig="), "{err}");
    }

    #[tokio::test]
    async fn an_answer_that_is_not_a_png_is_not_passed_on_as_the_page() {
        let docs = FakeDocs::start(axum::http::StatusCode::OK, "text/html").await;
        let mcp = located_behind(&docs).await;

        let err = mcp.cite_quote(ask_image()).await.unwrap_err();

        assert!(err.contains("text/html"), "{err}");
    }

    /// No boxes, no picture: the call says why instead of returning a
    /// citation that silently lacks the image that was asked for.
    #[tokio::test]
    async fn an_image_of_a_passage_that_is_not_located_is_refused() {
        let storage = FaultyStorage::new();
        let mcp = linked(&storage);
        seed(&mcp, &*storage, "pdf").await;

        let err = mcp.cite_quote(ask_image()).await.unwrap_err();

        assert!(err.contains("`pending`"), "{err}");
    }
}

mod openalex {
    use super::*;
    use crate::testkit::server_with_openalex;
    use crate::{OpenAlexCitationsParams, OpenAlexGetParams};

    fn ids(out: &str) -> Vec<String> {
        let rows: Vec<serde_json::Value> = serde_json::from_str(out).unwrap();
        let mut ids: Vec<String> = rows
            .iter()
            .map(|r| r["openalex_id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    fn citations(year_from: Option<u16>) -> Parameters<OpenAlexCitationsParams> {
        Parameters(OpenAlexCitationsParams {
            openalex_id: "W9".into(),
            limit: None,
            year_from,
            sort: None,
        })
    }

    /// RA-107: `publication_year >= COALESCE(?, 0)` dropped works with no year
    /// even when no year filter was given.
    #[tokio::test]
    async fn no_year_filter_keeps_works_without_a_year() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let all = mcp.openalex_citations(citations(None)).await.unwrap();
        assert_eq!(ids(&all), ["W1", "W2", "W3"]);

        let recent = mcp.openalex_citations(citations(Some(2000))).await.unwrap();
        assert_eq!(ids(&recent), ["W2"], "a real filter still filters");
    }

    /// A sort the tool does not offer is an error; it used to re-sort the
    /// results by the default key and answer a different question.
    #[tokio::test]
    async fn an_unknown_sort_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let err = mcp
            .openalex_citations(Parameters(OpenAlexCitationsParams {
                openalex_id: "W9".into(),
                limit: None,
                year_from: None,
                sort: Some("citation".into()),
            }))
            .await
            .unwrap_err();
        assert!(err.contains("Unknown sort"), "{err}");
    }

    #[tokio::test]
    async fn get_resolves_ids_and_normalizes_dois_on_the_query_side() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());
        for query in [
            "W1",
            "https://openalex.org/W1",
            "10.1234/ABC",
            "https://doi.org/10.1234/abc",
        ] {
            let out = mcp
                .openalex_get(Parameters(OpenAlexGetParams {
                    id_or_doi: query.into(),
                }))
                .await
                .unwrap();
            let out: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(out["openalex_id"], "W1", "{query}");
        }
        let missing = mcp
            .openalex_get(Parameters(OpenAlexGetParams {
                id_or_doi: "10.9999/none".into(),
            }))
            .await
            .unwrap();
        assert!(missing.contains("not_found"), "{missing}");
    }

    /// Requests do not share one locked connection: a handle held by a slow
    /// request does not stop the next one.
    #[tokio::test]
    async fn a_held_request_handle_does_not_block_other_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());
        let shared = mcp.openalex_db.as_ref().unwrap();
        let _slow = crate::openalex_request_conn(shared).unwrap();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mcp.openalex_citations(citations(None)),
        )
        .await
        .expect("must not wait for the other handle")
        .unwrap();
        assert_eq!(ids(&out).len(), 3);
    }
}

/// RA-7 / N6: the server starts without the broker; only the tools that
/// announce on it fail, and they fail loudly and before doing any work.
mod broker_outage {
    use super::*;
    use hs_common::event_bus::config::NatsYaml;
    use hs_common::event_bus::{EventBusConfig, EventsBackend};
    use hs_common::storage::Storage;

    /// A lazily connected NATS bus whose broker is a closed loopback port.
    async fn unreachable_bus() -> Arc<dyn hs_common::event_bus::EventBus> {
        let cfg = EventBusConfig {
            backend: EventsBackend::Nats,
            nats: NatsYaml {
                url: "nats://127.0.0.1:1".into(),
                ..NatsYaml::default()
            },
        };
        // Building it succeeds: the configuration is valid, nothing connects.
        EventBusConfig::build_lazy(Some(&cfg)).await.unwrap()
    }

    #[tokio::test]
    async fn read_only_tools_work_and_publishing_tools_error_loudly() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "paper-a",
            "# A\n\nBody text long enough to index.",
        )
        .await;
        let mcp = crate::testkit::server_with_event_bus(storage.clone(), unreachable_bus().await);

        // Read-only: unaffected by the outage.
        let listed = mcp
            .list_markdown(crate::ListParams {
                limit: None,
                offset: None,
                embedded: None,
            })
            .await
            .expect("read-only tools must not depend on the broker");
        assert!(listed.contains("paper-a"), "{listed}");

        // scribe_convert refuses before it touches the source.
        let err = mcp
            .convert_source(&crate::stem::Stem::parse("paper-a").unwrap())
            .await
            .unwrap_err();
        assert!(
            err.contains("scribe_convert") && err.contains("event bus"),
            "{err}"
        );
        assert!(err.contains("nothing was done"), "{err}");

        // paper_download refuses before any network request or storage write.
        let err = mcp
            .paper_download(Parameters(crate::PaperDownloadParams {
                doi: "10.1234/abc".into(),
            }))
            .await
            .unwrap_err();
        assert!(
            err.contains("paper_download") && err.contains("event bus"),
            "{err}"
        );
        assert!(
            !storage
                .exists("papers/10/10.1234_abc.pdf")
                .await
                .unwrap_or(false),
            "nothing may be stored"
        );
    }
}

mod paper_search {
    use super::*;
    use crate::PaperSearchParams;

    /// An unknown `sort` is refused before any provider is asked.
    #[tokio::test]
    async fn an_unknown_sort_is_refused() {
        let mcp = server(FaultyStorage::new(), None);
        let err = mcp
            .paper_search(Parameters(PaperSearchParams {
                query: "attention".into(),
                max_results: None,
                search_type: None,
                date: None,
                offset: None,
                provider: None,
                min_citations: None,
                sort: Some("citation".into()),
            }))
            .await
            .unwrap_err();
        assert!(err.contains("Unknown sort"), "{err}");
    }
}
