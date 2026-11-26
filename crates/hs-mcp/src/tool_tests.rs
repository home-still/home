//! Behavior of the tools against fake storage, a recording event bus, a
//! loopback distill server and a synthetic OpenAlex corpus.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;

use crate::testkit::{seed_catalog, seed_markdown, server, FakeDistill, FaultyStorage};

/// A document id, as a tool's arguments carry it.
fn doc(id: &str) -> crate::stem::Stem {
    crate::stem::Stem::parse(id).unwrap()
}

/// A tool's JSON answer.
fn parsed(out: &str) -> serde_json::Value {
    serde_json::from_str(out).unwrap()
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

mod read {
    use super::*;
    use crate::tools::read::ReadParams;
    use hs_common::catalog::CatalogEntry;
    use hs_common::storage::Storage;
    use hs_common::stores::{MEMORY, PERSONAL, REFERENCES};

    const DOC: &str = "paper-a";
    const TEXT: &str = "# A\n\nRivers carry sediment.\nDeltas grow.";

    fn ask(store: Option<&str>) -> ReadParams {
        ReadParams {
            store: store.map(str::to_string),
            doc_id: doc(DOC),
            page: None,
            lines: None,
            max_chars: None,
        }
    }

    async fn seeded() -> Arc<FaultyStorage> {
        let storage = FaultyStorage::new();
        seed_markdown(&*storage, DOC, TEXT).await;
        seed_catalog(&*storage, DOC).await;
        storage
    }

    #[tokio::test]
    async fn a_converted_document_comes_with_its_card_and_its_numbered_lines() {
        let mcp = server(seeded().await, None);

        let read = mcp
            .read_document(ReadParams {
                lines: Some([3, 4]),
                ..ask(None)
            })
            .await
            .unwrap();

        assert!(read.answer.converted);
        assert_eq!(read.answer.card.title.as_deref(), Some("A title"));
        assert_eq!(
            read.text.as_deref(),
            Some("3: Rivers carry sediment.\n4: Deltas grow.\n")
        );
        let slice = read.answer.slice.expect("a converted document has a slice");
        assert_eq!(
            (slice.line_start, slice.line_end, slice.total_lines),
            (3, 4, 4)
        );
        // No link signing is configured, so no links are offered.
        assert!(read.answer.links.is_none());
    }

    #[tokio::test]
    async fn a_document_that_is_not_converted_yet_says_so() {
        let storage = FaultyStorage::new();
        seed_catalog(&*storage, DOC).await;
        let mcp = server(storage, None);

        let read = mcp.read_document(ask(None)).await.unwrap();

        assert!(!read.answer.converted);
        assert!(read.answer.slice.is_none() && read.text.is_none());
        assert_eq!(read.answer.card.title.as_deref(), Some("A title"));
    }

    #[tokio::test]
    async fn a_document_the_store_does_not_hold_is_refused() {
        let mcp = server(FaultyStorage::new(), None);

        let err = mcp.read_document(ask(None)).await.unwrap_err();

        assert!(err.contains("has no document"), "{err}");
        assert!(err.contains("`documents`"), "{err}");
    }

    /// An outage must not read as "no such document": the caller would go
    /// and acquire a paper the store already holds.
    #[tokio::test]
    async fn a_storage_outage_is_an_error_not_a_missing_document() {
        let storage = seeded().await;
        let mcp = server(storage.clone(), None);
        FaultyStorage::set(&storage.fail_get, true);

        let err = mcp.read_document(ask(None)).await.unwrap_err();

        assert!(err.contains("failed"), "{err}");
        assert!(!err.contains("has no document"), "{err}");
    }

    /// A hit in `references` names the paper its reference list came from.
    #[tokio::test]
    async fn a_hit_in_a_derived_store_reads_the_document_it_belongs_to() {
        let mcp = server(seeded().await, None);

        let read = mcp.read_document(ask(Some(REFERENCES))).await.unwrap();

        assert_eq!(read.answer.store, REFERENCES);
        let text = read.text.expect("the paper's text");
        assert!(text.starts_with("1: # A\n"), "{text}");
    }

    /// The personal store keeps a document's card beside the document.
    #[tokio::test]
    async fn a_personal_document_is_read_with_the_card_beside_it() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);
        let (_, layout) = mcp.stores.documents(PERSONAL).unwrap();
        let card = CatalogEntry {
            title: Some("Field notes".into()),
            category: Some("notes".into()),
            ..Default::default()
        };
        for (key, bytes) in [
            (layout.markdown_key(DOC), TEXT.as_bytes().to_vec()),
            (
                layout.source_key(DOC, "catalog.yaml"),
                serde_yaml_ng::to_string(&card).unwrap().into_bytes(),
            ),
        ] {
            storage.put(&key, bytes).await.unwrap();
        }

        let read = mcp.read_document(ask(Some(PERSONAL))).await.unwrap();

        assert!(read.answer.converted);
        assert_eq!(read.answer.card.title.as_deref(), Some("Field notes"));
        assert_eq!(read.answer.card.category.as_deref(), Some("notes"));
    }

    #[tokio::test]
    async fn a_store_that_holds_no_documents_is_refused() {
        let mcp = server(seeded().await, None);

        let err = mcp.read_document(ask(Some(MEMORY))).await.unwrap_err();

        assert!(err.contains(MEMORY), "{err}");
    }
}

mod documents {
    use super::*;
    use crate::tools::documents::DocumentsParams;
    use crate::HomeStillMcp;
    use hs_common::catalog::{CatalogEntry, ConversionMeta, EmbeddingMeta};
    use hs_common::storage::Storage;
    use hs_common::stores::{ACADEMIC, MEMORY, PERSONAL, WEB};

    fn in_store(store: &str) -> DocumentsParams {
        DocumentsParams {
            store: Some(store.to_string()),
            ..Default::default()
        }
    }

    fn added(at: &str) -> CatalogEntry {
        CatalogEntry {
            title: Some(format!("Added {at}")),
            downloaded_at: Some(at.to_string()),
            ..Default::default()
        }
    }

    async fn file(
        mcp: &HomeStillMcp,
        storage: &dyn Storage,
        store: &str,
        doc_id: &str,
        entry: &CatalogEntry,
    ) {
        let (_, layout) = mcp.stores.documents(store).unwrap();
        hs_common::catalog::write_catalog_entry_via(storage, &layout.catalog_prefix, doc_id, entry)
            .await
            .unwrap();
    }

    fn ids(page: &serde_json::Value) -> Vec<&str> {
        page["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["doc_id"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_store_is_listed_newest_first_with_how_far_each_document_got() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);
        file(
            &mcp,
            &*storage,
            ACADEMIC,
            "older",
            &added("2026-01-01T00:00:00Z"),
        )
        .await;
        let indexed = CatalogEntry {
            conversion: Some(ConversionMeta {
                server: "test".into(),
                duration_secs: 0.0,
                total_pages: 1,
                converted_at: "2026-02-01T01:00:00Z".into(),
                pages: Vec::new(),
                converted_by: None,
                attempts_log: Vec::new(),
            }),
            embedding: Some(EmbeddingMeta {
                server: "test".into(),
                chunks_indexed: 5,
                compute_device: "cuda".into(),
                embedded_at: "2026-02-01T02:00:00Z".into(),
            }),
            ..added("2026-02-01T00:00:00Z")
        };
        file(&mcp, &*storage, ACADEMIC, "newer", &indexed).await;

        let page = parsed(
            &mcp.list_documents(DocumentsParams::default())
                .await
                .unwrap(),
        );

        assert_eq!(page["store"], ACADEMIC);
        assert_eq!(page["matching"], 2);
        assert_eq!(ids(&page), ["newer", "older"]);
        let stage = |row: usize| {
            (
                page["documents"][row]["converted"].as_bool(),
                page["documents"][row]["indexed"].as_bool(),
            )
        };
        assert_eq!(stage(0), (Some(true), Some(true)));
        assert_eq!(stage(1), (Some(false), Some(false)));
    }

    /// A page captured from the web is not a paper, and a personal document
    /// is listed from the card beside it.
    #[tokio::test]
    async fn each_store_lists_only_its_own_documents() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);
        let at = added("2026-01-01T00:00:00Z");
        file(&mcp, &*storage, ACADEMIC, "paper", &at).await;
        file(&mcp, &*storage, WEB, "page", &at).await;
        let (_, personal) = mcp.stores.documents(PERSONAL).unwrap();
        storage
            .put(
                &personal.source_key("note", "catalog.yaml"),
                serde_yaml_ng::to_string(&at).unwrap().into_bytes(),
            )
            .await
            .unwrap();

        for (store, holds) in [(ACADEMIC, "paper"), (WEB, "page"), (PERSONAL, "note")] {
            let page = parsed(&mcp.list_documents(in_store(store)).await.unwrap());
            assert_eq!(ids(&page), [holds], "the store {store}");
        }
    }

    #[tokio::test]
    async fn a_store_without_documents_and_an_unknown_store_are_refused() {
        let mcp = server(FaultyStorage::new(), None);

        for store in [MEMORY, "nonesuch"] {
            let err = mcp.list_documents(in_store(store)).await.unwrap_err();
            assert!(err.contains(store), "{err}");
        }
    }

    /// An outage must not read as an empty store.
    #[tokio::test]
    async fn a_listing_that_fails_is_an_error_not_an_empty_store() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);
        file(
            &mcp,
            &*storage,
            ACADEMIC,
            "paper",
            &added("2026-01-01T00:00:00Z"),
        )
        .await;
        FaultyStorage::set(&storage.fail_list, true);

        let err = mcp
            .list_documents(DocumentsParams::default())
            .await
            .unwrap_err();

        assert!(err.contains("failed"), "{err}");
    }
}

mod capture {
    use super::*;
    use crate::testkit::{server_with_bus, RecordingBus};
    use crate::tools::capture::{CaptureContentType, CaptureParams};
    use hs_common::event_bus::specs::PAPERS_INGESTED;
    use hs_common::storage::Storage;
    use hs_common::stores::WEB;

    const ADDRESS: &str = "https://example.org/rivers";
    const PAGE: &str = "<html><body><article><h1>Rivers</h1>\
        <p>Rivers carry sediment from the mountains to the sea.</p></article></body></html>";

    /// A capture of [`ADDRESS`], asked for by a link to a part of the page.
    fn page(content: &str, content_type: CaptureContentType) -> CaptureParams {
        CaptureParams {
            url: format!("{ADDRESS}#delta"),
            content: content.to_string(),
            content_type,
            title: Some("Rivers".into()),
        }
    }

    #[tokio::test]
    async fn a_page_is_stored_under_its_address_and_announced_for_conversion() {
        let storage = FaultyStorage::new();
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = parsed(
            &mcp.capture_page(page(PAGE, CaptureContentType::Html))
                .await
                .unwrap(),
        );

        assert_eq!(out["store"], WEB);
        assert_eq!(out["source_url"], ADDRESS, "the fragment is not the page");
        assert_eq!(out["replaced"], false);
        let doc_id = out["doc_id"].as_str().unwrap();
        let (_, layout) = mcp.stores.documents(WEB).unwrap();
        let key = layout.source_key(doc_id, "html");
        assert_eq!(storage.get(&key).await.unwrap(), PAGE.as_bytes());
        let entry =
            hs_common::catalog::read_catalog_entry_via(&*storage, &layout.catalog_prefix, doc_id)
                .await
                .unwrap()
                .expect("the page has a catalog entry");
        assert_eq!(entry.title.as_deref(), Some("Rivers"));
        assert_eq!(entry.source_url.as_deref(), Some(ADDRESS));

        let published = bus.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, PAPERS_INGESTED.subject);
        assert_eq!(published[0].1["key"], key);
        assert_eq!(published[0].1["source"], "mcp:capture");
    }

    /// The page at an address is one document. Capturing it again replaces
    /// the earlier capture and the text converted from it, which would
    /// otherwise stop the new page from being converted.
    #[tokio::test]
    async fn capturing_an_address_again_replaces_the_page_and_its_converted_text() {
        let storage = FaultyStorage::new();
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());
        let first = parsed(
            &mcp.capture_page(page(PAGE, CaptureContentType::Html))
                .await
                .unwrap(),
        );
        let doc_id = first["doc_id"].as_str().unwrap();
        let (_, layout) = mcp.stores.documents(WEB).unwrap();
        storage
            .put(&layout.markdown_key(doc_id), b"# Rivers\n".to_vec())
            .await
            .unwrap();

        let second = parsed(
            &mcp.capture_page(page(
                "# Rivers\n\nThe river moved.",
                CaptureContentType::Markdown,
            ))
            .await
            .unwrap(),
        );

        assert_eq!(second["doc_id"], first["doc_id"]);
        assert_eq!(second["replaced"], true);
        for (key, kept) in [
            (layout.source_key(doc_id, "html"), false),
            (layout.markdown_key(doc_id), false),
            (layout.source_key(doc_id, "md"), true),
        ] {
            assert_eq!(storage.exists(&key).await.unwrap(), kept, "{key}");
        }
        assert_eq!(bus.published.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_paywall_page_is_refused_with_nothing_stored_or_announced() {
        let storage = FaultyStorage::new();
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());
        let wall = "<html><body><p>Sign in to read the full text. \
                    Subscription required.</p></body></html>";

        let err = mcp
            .capture_page(page(wall, CaptureContentType::Html))
            .await
            .unwrap_err();

        assert!(err.contains("paywall"), "{err}");
        let (_, layout) = mcp.stores.documents(WEB).unwrap();
        assert!(storage
            .list(&layout.source_prefix)
            .await
            .unwrap()
            .is_empty());
        assert!(bus.published.lock().unwrap().is_empty());
    }
}

mod synthesis_save {
    use super::*;
    use crate::testkit::{server_with_bus, RecordingBus};
    use crate::tools::synthesis_save::{SynthesisCitation, SynthesisSaveParams};
    use crate::HomeStillMcp;
    use hs_common::storage::Storage;
    use hs_common::stores::{ACADEMIC, SYNTHESES};

    const DOC: &str = "paper-a";
    const TEXT: &str =
        "# Rivers\n\nThe delta grows where the current slows and the silt settles out.";
    const QUOTE: &str = "the current slows and the silt settles out";
    /// [`QUOTE`] with one word the paper does not say.
    const MISQUOTE: &str = "the current slows and the mud settles out";
    const BODY: &str = "Deltas grow where rivers slow.";

    /// A server whose academic store holds the paper the quotes are from.
    async fn with_paper() -> (Arc<FaultyStorage>, Arc<RecordingBus>, HomeStillMcp) {
        let storage = FaultyStorage::new();
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        for (key, bytes) in [
            (layout.source_key(DOC, "html"), b"the source".to_vec()),
            (layout.markdown_key(DOC), TEXT.as_bytes().to_vec()),
        ] {
            storage.put(&key, bytes).await.unwrap();
        }
        (storage, bus, mcp)
    }

    fn citing(quotes: &[&str]) -> SynthesisSaveParams {
        SynthesisSaveParams {
            title: "How deltas grow".into(),
            body: BODY.into(),
            citations: quotes
                .iter()
                .map(|quote| SynthesisCitation {
                    store: ACADEMIC.into(),
                    doc_id: doc(DOC),
                    quote: quote.to_string(),
                })
                .collect(),
            tags: Some(vec!["rivers".into()]),
        }
    }

    #[tokio::test]
    async fn a_synthesis_is_saved_with_the_passages_it_stands_on() {
        let (storage, bus, mcp) = with_paper().await;

        let out = parsed(&mcp.save_synthesis(citing(&[QUOTE])).await.unwrap());

        assert_eq!(out["store"], SYNTHESES);
        assert_eq!(out["citations_verified"], 1);
        let doc_id = out["doc_id"].as_str().unwrap();
        assert!(doc_id.ends_with("-how-deltas-grow"), "{doc_id}");
        let (_, layout) = mcp.stores.documents(SYNTHESES).unwrap();
        let key = layout.source_key(doc_id, "md");
        let saved = String::from_utf8(storage.get(&key).await.unwrap()).unwrap();
        let (front, body) = saved
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("---\n"))
            .expect("front matter, then the text");
        let front: serde_json::Value = serde_yaml_ng::from_str(front).unwrap();
        assert_eq!(front["title"], "How deltas grow");
        assert_eq!(front["tags"], serde_json::json!(["rivers"]));
        assert_eq!(
            front["citations"],
            serde_json::json!([{
                "store": ACADEMIC,
                "doc_id": DOC,
                "lines": [3, 3],
                "quote": QUOTE,
            }])
        );
        assert_eq!(body.trim(), BODY);

        let published = bus.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].1["key"], key);
        assert_eq!(published[0].1["source"], "mcp:synthesis_save");
    }

    /// A synthesis is only as good as its weakest citation: one that the
    /// document does not say stops the whole save.
    #[tokio::test]
    async fn one_citation_that_is_not_in_its_document_saves_nothing() {
        let (storage, bus, mcp) = with_paper().await;

        let err = mcp
            .save_synthesis(citing(&[QUOTE, MISQUOTE]))
            .await
            .unwrap_err();

        assert!(err.contains("citation 2 of 2"), "{err}");
        assert!(err.contains("nothing was saved"), "{err}");
        let (_, layout) = mcp.stores.documents(SYNTHESES).unwrap();
        assert!(storage
            .list(&layout.source_prefix)
            .await
            .unwrap()
            .is_empty());
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_synthesis_that_cites_nothing_is_refused() {
        let (_, bus, mcp) = with_paper().await;

        let err = mcp.save_synthesis(citing(&[])).await.unwrap_err();

        assert!(err.contains("`citations` is empty"), "{err}");
        assert!(bus.published.lock().unwrap().is_empty());
    }
}

mod status {
    use super::*;
    use crate::testkit::FAKE_RECORDS_PER_DOC;
    use crate::tools::status::StatusParams;
    use hs_common::storage::Storage;
    use hs_common::stores::{ABSTRACTS, ACADEMIC, REFERENCES};

    const DOC: &str = "paper-a";

    fn of(doc_id: &str) -> StatusParams {
        StatusParams {
            store: None,
            doc_id: Some(doc(doc_id)),
        }
    }

    #[tokio::test]
    async fn a_document_is_reported_stage_by_stage() {
        let storage = FaultyStorage::new();
        let distill = FakeDistill::start(vec![DOC.to_string()]).await;
        let mcp = server(storage.clone(), Some(&distill));
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        storage
            .put(&layout.source_key(DOC, "pdf"), b"%PDF".to_vec())
            .await
            .unwrap();
        seed_markdown(&*storage, DOC, "# A\n\nBody.").await;
        seed_catalog(&*storage, DOC).await;

        let out = parsed(&mcp.report_status(of(DOC)).await.unwrap());

        assert_eq!(
            out["source"],
            serde_json::json!({ "stored": true, "extension": "pdf" })
        );
        assert_eq!(out["catalog_entry"], true);
        assert_eq!(out["converted"], true);
        assert_eq!(out["boxes"], "pending", "a PDF whose boxes are not built");
        assert!(out["failure"].is_null(), "{out}");
        // The paper's own store first, then the stores made from it.
        let records = out["records"].as_array().unwrap();
        assert_eq!(
            records[0],
            serde_json::json!({ "store": ACADEMIC, "records": FAKE_RECORDS_PER_DOC })
        );
        let stores: Vec<&str> = records
            .iter()
            .map(|r| r["store"].as_str().unwrap())
            .collect();
        assert!(
            stores.contains(&REFERENCES) && stores.contains(&ABSTRACTS),
            "{stores:?}"
        );
    }

    /// "Not here yet" is an answer, and an index that cannot be asked says
    /// so instead of reporting zero records.
    #[tokio::test]
    async fn a_document_nothing_is_known_about_is_reported_absent() {
        let mcp = server(FaultyStorage::new(), None);

        let out = parsed(&mcp.report_status(of("nonesuch")).await.unwrap());

        assert_eq!(out["source"]["stored"], false);
        assert_eq!(out["catalog_entry"], false);
        assert_eq!(out["converted"], false);
        assert_eq!(out["boxes"], "not_applicable");
        let unknown = out["records"][0]["records"]["error"]
            .as_str()
            .unwrap_or_default();
        assert!(unknown.contains("no distill server"), "{out}");
    }

    #[tokio::test]
    async fn a_recorded_conversion_failure_is_reported_with_its_reason() {
        let storage = FaultyStorage::new();
        let mcp = server(storage.clone(), None);
        let (_, layout) = mcp.stores.documents(ACADEMIC).unwrap();
        hs_common::catalog::update_conversion_failed_via(
            &*storage,
            &layout.catalog_prefix,
            DOC,
            "unsupported_content_type:html",
            Vec::new(),
        )
        .await
        .unwrap();

        let out = parsed(&mcp.report_status(of(DOC)).await.unwrap());

        assert_eq!(out["converted"], false);
        assert_eq!(out["failure"]["stage"], "conversion");
        assert_eq!(out["failure"]["reason"], "unsupported_content_type:html");
    }

    /// One part that cannot answer does not cost the others their answer.
    #[tokio::test]
    async fn the_pipeline_report_says_which_part_could_not_answer() {
        let distill = FakeDistill::start(vec![DOC.to_string()]).await;
        let mcp = server(FaultyStorage::new(), Some(&distill));

        let out = parsed(&mcp.report_status(StatusParams::default()).await.unwrap());

        assert_eq!(out["index"]["store"], ACADEMIC);
        assert_eq!(out["index"]["documents"], 1);
        // The test's bus cannot report depths: each queue says so instead
        // of reporting an empty queue.
        let queues = out["queues"].as_array().unwrap();
        assert_eq!(queues.len(), 3);
        assert!(
            queues.iter().all(|q| q["depth"]["error"].is_string()),
            "{out}"
        );
        assert_eq!(out["converters"], serde_json::json!([]));
    }
}

/// `discover` and `graph` against a synthetic OpenAlex corpus.
mod corpus {
    use super::*;
    use crate::testkit::server_with_openalex;
    use crate::tools::discover::{DiscoverKind, DiscoverParams, DiscoverSource};
    use crate::tools::graph::{GraphDirection, GraphParams, GraphSort, GraphSource};

    /// The work W1, W2 and W3 cite.
    const CITED: &str = "10.1234/cited";
    /// W1, which cites [`CITED`].
    const CITING: &str = "10.1234/abc";

    fn neighbours(doi: &str, direction: GraphDirection) -> GraphParams {
        GraphParams {
            doi: doi.to_string(),
            direction,
            source: GraphSource::Openalex,
            limit: None,
            year_from: None,
            sort: GraphSort::default(),
        }
    }

    fn ids(out: &str) -> Vec<String> {
        parsed(out)["works"]
            .as_array()
            .unwrap()
            .iter()
            .map(|work| work["openalex_id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn citations_are_the_works_that_cite_and_references_the_works_cited() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let citing = mcp
            .graph_neighbours(neighbours(CITED, GraphDirection::Citations))
            .await
            .unwrap();
        // Newest first, the undated work last.
        assert_eq!(ids(&citing), ["W2", "W1", "W3"]);

        let cited = mcp
            .graph_neighbours(neighbours(CITING, GraphDirection::References))
            .await
            .unwrap();
        assert_eq!(ids(&cited), ["W9"]);
    }

    #[tokio::test]
    async fn a_year_floor_drops_older_and_undated_works() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let out = mcp
            .graph_neighbours(GraphParams {
                year_from: Some(2000),
                ..neighbours(CITED, GraphDirection::Citations)
            })
            .await
            .unwrap();

        assert_eq!(ids(&out), ["W2"]);
    }

    #[tokio::test]
    async fn the_limit_cuts_the_list_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());
        let limited = |limit: u32| GraphParams {
            limit: Some(limit),
            ..neighbours(CITED, GraphDirection::Citations)
        };

        let cut = mcp.graph_neighbours(limited(2)).await.unwrap();
        assert_eq!(ids(&cut), ["W2", "W1"]);
        assert_eq!(parsed(&cut)["truncated"], true);

        let whole = mcp.graph_neighbours(limited(3)).await.unwrap();
        assert_eq!(ids(&whole).len(), 3);
        assert_eq!(parsed(&whole)["truncated"], false);
    }

    #[tokio::test]
    async fn a_doi_the_corpus_does_not_hold_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let err = mcp
            .graph_neighbours(neighbours("10.9999/nonesuch", GraphDirection::Citations))
            .await
            .unwrap_err();

        assert!(err.contains("has no work with DOI"), "{err}");
    }

    #[tokio::test]
    async fn a_work_is_looked_up_in_the_corpus_by_its_doi() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());

        let out = parsed(
            &mcp.discover_works(DiscoverParams {
                source: DiscoverSource::Openalex,
                query: None,
                doi: Some(CITING.to_string()),
                kind: DiscoverKind::Works,
                year_from: None,
                year_to: None,
                limit: None,
            })
            .await
            .unwrap(),
        );

        assert_eq!(out["work"]["openalex_id"], "W1");
        assert_eq!(out["work"]["title"], "Old paper");
        assert_eq!(out["work"]["authors"], serde_json::json!([]));
    }

    /// A server without the corpus still serves every other tool; the two
    /// that read it pass on why it is not there.
    #[tokio::test]
    async fn without_the_corpus_its_tools_say_how_to_build_it() {
        let mcp = server(FaultyStorage::new(), None);

        let err = mcp
            .graph_neighbours(neighbours(CITING, GraphDirection::References))
            .await
            .unwrap_err();

        assert!(err.contains("hs openalex load-works"), "{err}");
    }

    /// Requests do not share one locked connection: a handle held by a slow
    /// request does not stop the next one.
    #[tokio::test]
    async fn a_held_request_handle_does_not_block_other_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = server_with_openalex(dir.path());
        let _slow = mcp.openalex.connection().unwrap();

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mcp.graph_neighbours(neighbours(CITED, GraphDirection::Citations)),
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
    use crate::tools::acquire::AcquireParams;
    use crate::tools::capture::{CaptureContentType, CaptureParams};
    use crate::tools::documents::DocumentsParams;
    use crate::tools::read::ReadParams;
    use crate::tools::synthesis_save::{SynthesisCitation, SynthesisSaveParams};
    use hs_common::event_bus::config::NatsYaml;
    use hs_common::event_bus::{EventBusConfig, EventsBackend};
    use hs_common::storage::Storage;
    use hs_common::stores::{ACADEMIC, SYNTHESES, WEB};

    const DOC: &str = "paper-a";

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
    async fn reading_works_and_every_tool_that_announces_refuses_before_doing_anything() {
        let storage = FaultyStorage::new();
        seed_markdown(&*storage, DOC, "# A\n\nBody text long enough to quote.").await;
        seed_catalog(&*storage, DOC).await;
        let mcp = crate::testkit::server_with_event_bus(storage.clone(), unreachable_bus().await);

        // Read-only: unaffected by the outage.
        let read = mcp
            .read_document(ReadParams {
                store: None,
                doc_id: doc(DOC),
                page: None,
                lines: None,
                max_chars: None,
            })
            .await
            .expect("reading must not depend on the broker");
        assert!(read.answer.converted);
        let listed = mcp
            .list_documents(DocumentsParams::default())
            .await
            .expect("listing must not depend on the broker");
        assert!(listed.contains(DOC), "{listed}");

        let refusals = [
            (
                "acquire",
                mcp.acquire_paper(AcquireParams {
                    doi: "10.1234/abc".into(),
                })
                .await,
            ),
            (
                "capture",
                mcp.capture_page(CaptureParams {
                    url: "https://example.org/rivers".into(),
                    content: "# Rivers\n\nRivers carry sediment.".into(),
                    content_type: CaptureContentType::Markdown,
                    title: None,
                })
                .await,
            ),
            (
                "synthesis_save",
                mcp.save_synthesis(SynthesisSaveParams {
                    title: "Rivers".into(),
                    body: "Rivers carry sediment.".into(),
                    citations: vec![SynthesisCitation {
                        store: ACADEMIC.into(),
                        doc_id: doc(DOC),
                        quote: "Body text long enough to quote".into(),
                    }],
                    tags: None,
                })
                .await,
            ),
        ];
        for (tool, outcome) in refusals {
            let err = outcome.unwrap_err();
            assert!(err.contains(tool) && err.contains("event bus"), "{err}");
            assert!(err.contains("nothing was done"), "{err}");
        }

        // No download, no page and no synthesis was stored.
        assert!(
            !storage
                .exists("papers/10/10.1234_abc.pdf")
                .await
                .unwrap_or(false),
            "nothing may be stored"
        );
        for store in [WEB, SYNTHESES] {
            let (_, layout) = mcp.stores.documents(store).unwrap();
            let stored = storage.list(&layout.source_prefix).await.unwrap();
            assert!(stored.is_empty(), "the store {store} holds {stored:?}");
        }
    }
}
