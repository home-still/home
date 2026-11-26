use hs_common::catalog::{CatalogEntry, PageOffset};

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::client::DistillProgress;
use crate::config::DistillServerConfig;
use crate::error::DistillError;
use crate::metadata::extract_rule_based;
use crate::store::DocumentStore;

/// What kind of content a collection deliberately carries — decides which
/// ingress quality gates `index_document` applies. The mapping lives in
/// exactly one place ([`ContentProfile::for_collection`]); adding a new
/// deliberately-short collection means adding it THERE, not discovering
/// scattered name-compares after its content silently drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentProfile {
    /// Full-body converted documents: gate on the paywall/interstitial
    /// stub heuristics and drop low-quality chunks.
    FullDocument,
    /// Deliberately short payloads (paper abstracts). The "short page
    /// without article structure = junk" and 50-char `TooShort` rules
    /// would reject exactly the content these collections exist to carry,
    /// so both gates are off — the producing pipeline already decided
    /// what is worth indexing.
    ShortFormCurated,
}

impl ContentProfile {
    /// The one registry mapping collection names to profiles.
    pub fn for_collection(collection_name: &str) -> Self {
        match collection_name {
            "paper_abstracts" => Self::ShortFormCurated,
            _ => Self::FullDocument,
        }
    }
}

/// One document to index. The text is always supplied by the caller; this
/// module never reads documents from disk.
pub struct IndexJob<'a> {
    /// Identity of the document in the collection (the catalog stem).
    pub doc_id: &'a str,
    /// The caller's name for the document (a storage key); recorded in the
    /// payload as `markdown_path`. Not opened.
    pub markdown_path: &'a str,
    pub content: &'a str,
    /// The document's catalog entry, if the caller has one. It is the only
    /// source of title/authors/DOI/year: the server has no catalog of its
    /// own.
    pub catalog: Option<CatalogEntry>,
    pub collection: &'a str,
    pub profile: ContentProfile,
}

/// Index a single markdown document: chunk -> metadata -> write.
///
/// After this returns `Ok(n)` the collection holds exactly the `n` chunks
/// produced from `job.content` for `doc_id`, and none left over from an
/// earlier version of the document:
///
/// * `n > 0`: the new generation is written first, and only after the write
///   succeeded are the records of every earlier generation deleted (all
///   records of the document except the ones just written). A failed write
///   therefore never leaves a document with fewer chunks than before, and a
///   stale generation never outlives a re-index. Records a failed write did
///   leave behind stay until the next successful index of the document
///   removes them, and the caller records no success, so the document is
///   retried.
/// * `n == 0` (empty document, interstitial stub, no chunk passed the
///   quality filter): every earlier record of the document is deleted before
///   `Ok(0)` is returned, so a caller that stamps the document as skipped
///   is not contradicted by searchable chunks.
///
/// Any failure — including the deletes — is an `Err` and changes nothing
/// about what the caller should record.
pub async fn index_document(
    job: IndexJob<'_>,
    config: &DistillServerConfig,
    store: &dyn DocumentStore,
    on_progress: impl Fn(DistillProgress),
) -> Result<u32, DistillError> {
    let IndexJob {
        doc_id: stem,
        markdown_path,
        content: markdown,
        catalog,
        collection: collection_name,
        profile,
    } = job;

    on_progress(DistillProgress {
        stage: "reading".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: 0,
        message: format!("Reading {stem}"),
    });

    if markdown.trim().is_empty() {
        tracing::warn!("Skipping empty document: {}", stem);
        remove_document(store, collection_name, stem).await?;
        return Ok(0);
    }

    // Belt-and-suspenders: refuse markdown that matches a *known* anti-bot /
    // cookie-wall interstitial signature even though the downloader and
    // scribe already gate at ingress. A new origin-side anti-bot variant or
    // a pre-rc.315 residual stub would otherwise be embedded as a 1-chunk
    // vector and poison search. Returning Ok(0) routes through
    // record_embedding_outcome_via → embedding_skip = zero_chunks_or_empty,
    // which the reconciler treats as an intentional terminal skip.
    //
    // This gate uses `is_known_interstitial`, NOT `is_paywall_html`. The
    // latter is an *HTML* heuristic — it strips tags, looks for `<article`,
    // and rejects any page under 100 KB that says "sign in", or any page
    // mentioning "clinical trials" / "search results" that lacks a literal
    // "abstract"+"references" pair. Those rules are calibrated for raw HTML
    // at download time (see hs-scribe's html arm), where a false positive
    // just costs a re-download. Run against *converted markdown* they are a
    // category error, and the cost of a false positive is permanent
    // invisibility: the doc is stamped terminal-skip and never reaches
    // search. Measured on the corpus 2026-08-10, that gate was silently
    // discarding 133 complete papers — including the full text of
    // "Accelerate" (399 KB) and a 99 KB mathematics-education paper —
    // while its own conservative sibling cleared 277 of the 278 documents
    // it rejected. `is_known_interstitial` matches only literal, unique
    // interstitial boilerplate, and is documented as safe where a false
    // positive would destroy real content.
    if profile == ContentProfile::FullDocument && hs_common::html::is_known_interstitial(markdown) {
        tracing::warn!(
            stem,
            len = markdown.len(),
            "Skipping paywall/interstitial markdown stub"
        );
        remove_document(store, collection_name, stem).await?;
        return Ok(0);
    }

    let page_offsets: Vec<PageOffset> = catalog
        .as_ref()
        .and_then(|c| c.conversion.as_ref())
        .map(|conv| conv.pages.clone())
        .unwrap_or_default();

    // Extract metadata
    on_progress(DistillProgress {
        stage: "metadata".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: 0,
        message: "Extracting metadata".into(),
    });

    let mut meta = extract_rule_based(markdown, stem, markdown_path, catalog.as_ref());
    // pdf_path is always populated as a sharded storage key by
    // `extract_rule_based`; no host-filesystem fallback.

    // Optional LLM metadata extraction. A failure fails the document
    // (surfaced to the caller, retried by the event bus) rather than
    // indexing it without the keywords the operator asked for. A reply
    // with an empty list never replaces metadata that is already present.
    if config.llm_metadata {
        let llm = crate::metadata::extract_llm_metadata(
            markdown,
            &config.ollama_url,
            &config.metadata_model,
            std::time::Duration::from_secs(config.ollama_timeout_secs),
        )
        .await?;
        if !llm.keywords.is_empty() {
            meta.keywords = llm.keywords;
        }
        if !llm.topics.is_empty() {
            meta.topics = llm.topics;
        }
    }

    // Chunk
    on_progress(DistillProgress {
        stage: "chunking".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: 0,
        message: "Chunking document".into(),
    });

    let chunker_config = ChunkerConfig {
        max_tokens: config.chunk_max_tokens,
        overlap_tokens: config.chunk_overlap,
        ..Default::default()
    };

    let chunks = chunk_markdown(markdown, &meta, &page_offsets, &chunker_config)?;

    // Filter out low-quality chunks (repetition loops, garbled text, etc.)
    // Short-form collections bypass the filter — their single-chunk
    // payloads fail the 50-char `TooShort` rule by design, and dropping
    // points behind the producer's back was causing 2,268 catalog stamps
    // to point at records that do not exist. See ContentProfile.
    let mut chunks: Vec<_> = if profile == ContentProfile::ShortFormCurated {
        chunks
    } else {
        let pre_filter = chunks.len();
        let kept: Vec<_> = chunks
            .into_iter()
            .filter(|c| !crate::quality::is_low_quality(&c.raw_text))
            .collect();
        let filtered = pre_filter - kept.len();
        if filtered > 0 {
            tracing::info!("{}: skipped {} low-quality chunk(s)", stem, filtered);
        }
        kept
    };

    if chunks.is_empty() {
        tracing::warn!("No chunks produced for {}", stem);
        remove_document(store, collection_name, stem).await?;
        return Ok(0);
    }

    // Surviving chunks are renumbered so indices are contiguous 0..n: the
    // quality filter can drop chunks from the middle.
    let total_chunks = chunks.len() as u32;
    for (i, chunk) in chunks.iter_mut().enumerate() {
        chunk.chunk_index = i as u32;
        chunk.total_chunks = total_chunks;
    }

    // Write. The store embeds each chunk (header included) and keeps the
    // text verbatim.
    on_progress(DistillProgress {
        stage: "writing".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: total_chunks as u64,
        message: format!("Writing {} chunks", total_chunks),
    });

    let record_ids = store.write_document(collection_name, stem, &chunks).await?;

    // Only now that every new chunk is stored: drop what earlier generations
    // of this document left.
    store
        .forget_document(collection_name, stem, &record_ids)
        .await?;

    on_progress(DistillProgress {
        stage: "done".into(),
        doc: stem.to_string(),
        chunks_done: total_chunks as u64,
        chunks_total: total_chunks as u64,
        message: format!("Indexed {} chunks", total_chunks),
    });

    Ok(total_chunks)
}

/// Remove every stored chunk of a document that is being skipped.
async fn remove_document(
    store: &dyn DocumentStore,
    collection: &str,
    doc_id: &str,
) -> Result<(), DistillError> {
    store.forget_document(collection, doc_id, &[]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{prose, small_chunk_config, FakeStore, Op};

    const COLLECTION: &str = "academic_papers";

    async fn index(
        store: &FakeStore,
        config: &DistillServerConfig,
        doc_id: &str,
        content: &str,
    ) -> Result<u32, DistillError> {
        index_with(
            store,
            config,
            doc_id,
            content,
            ContentProfile::FullDocument,
            COLLECTION,
        )
        .await
    }

    async fn index_with(
        store: &FakeStore,
        config: &DistillServerConfig,
        doc_id: &str,
        content: &str,
        profile: ContentProfile,
        collection: &str,
    ) -> Result<u32, DistillError> {
        index_document(
            IndexJob {
                doc_id,
                markdown_path: &format!("markdown/xx/{doc_id}.md"),
                content,
                catalog: None,
                collection,
                profile,
            },
            config,
            store,
            |_| {},
        )
        .await
    }

    fn range(n: u32) -> Vec<u32> {
        (0..n).collect()
    }

    #[tokio::test]
    async fn reindexing_with_fewer_chunks_leaves_no_stale_generation() {
        // RA-42: the old code upserted `doc:0..n` and never deleted `doc:n..`.
        let (store, cfg) = (FakeStore::default(), small_chunk_config());

        let long = index(&store, &cfg, "doc", &prose(30)).await.unwrap();
        assert!(long >= 6, "need a multi-chunk document, got {long}");
        assert_eq!(store.stored(COLLECTION, "doc"), range(long));

        let short = index(&store, &cfg, "doc", &prose(8)).await.unwrap();
        assert!(short < long);
        assert_eq!(
            store.stored(COLLECTION, "doc"),
            range(short),
            "records of the earlier generation must be gone"
        );
    }

    #[tokio::test]
    async fn the_old_generation_is_deleted_only_after_the_write_succeeded_and_never_the_new_one() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(COLLECTION, "doc", 20);

        let n = index(&store, &cfg, "doc", &prose(12)).await.unwrap();
        assert!(n > 2);

        let ops = store.ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        let Op::Write { indexes, .. } = &ops[0] else {
            panic!("the write comes first: {ops:?}");
        };
        assert_eq!(*indexes, range(n));
        let Op::Forget {
            collection,
            doc_id,
            keep,
        } = &ops[1]
        else {
            panic!("the cleanup comes last: {ops:?}");
        };
        assert_eq!((collection.as_str(), doc_id.as_str()), (COLLECTION, "doc"));
        assert_eq!(keep.len(), n as usize, "keep exactly the new records");
        assert_eq!(store.stored(COLLECTION, "doc"), range(n));
    }

    #[tokio::test]
    async fn a_failed_write_never_deletes_the_previous_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(COLLECTION, "doc", 10);

        store.state.lock().fail_write_from_call = Some(0);
        let err = index(&store, &cfg, "doc", &prose(12)).await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)));

        assert!(
            !store.ops().iter().any(|op| matches!(op, Op::Forget { .. })),
            "no delete may run when the write failed: {:?}",
            store.ops()
        );
        assert_eq!(
            store.stored(COLLECTION, "doc"),
            range(10),
            "the document must not lose chunks because its re-index failed"
        );
    }

    #[tokio::test]
    async fn a_failed_cleanup_after_a_good_write_is_an_error_not_a_success() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(COLLECTION, "doc", 3);
        store.state.lock().fail_forget = true;
        let err = index(&store, &cfg, "doc", &prose(12)).await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    #[tokio::test]
    async fn every_skip_path_removes_the_documents_old_chunks() {
        // RA-42: each `Ok(0)` used to leave earlier chunks searchable while
        // the caller stamped the document as skipped.
        let cfg = small_chunk_config();
        let stub = "Checking your browser before accessing the site. Please wait.";
        assert!(
            hs_common::html::is_known_interstitial(stub),
            "fixture must be an interstitial"
        );

        for (label, content) in [
            ("empty", ""),
            ("whitespace", "  \n\t \n"),
            ("interstitial", stub),
            ("no chunk passes the quality filter", "tiny"),
        ] {
            let store = FakeStore::default();
            store.seed(COLLECTION, "doc", 7);
            let n = index(&store, &cfg, "doc", content).await.unwrap();
            assert_eq!(n, 0, "{label}");
            assert!(
                store.stored(COLLECTION, "doc").is_empty(),
                "{label}: old chunks still stored"
            );
            assert!(
                store.state.lock().written.is_empty(),
                "{label}: nothing should be written"
            );
        }
    }

    #[tokio::test]
    async fn skipping_another_document_does_not_touch_this_one() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(COLLECTION, "keep", 4);
        store.seed(COLLECTION, "skip", 4);
        index(&store, &cfg, "skip", "").await.unwrap();
        assert_eq!(store.stored(COLLECTION, "keep"), range(4));
        assert!(store.stored(COLLECTION, "skip").is_empty());
    }

    #[tokio::test]
    async fn a_skip_whose_cleanup_fails_is_an_error_not_a_recorded_skip() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(COLLECTION, "doc", 3);
        store.state.lock().fail_forget = true;
        let err = index(&store, &cfg, "doc", "").await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)));
    }

    #[tokio::test]
    async fn chunk_indexes_stay_contiguous_when_the_quality_filter_drops_a_middle_chunk() {
        // Three pages -> three chunks; the middle one is a repetition loop.
        // The survivors must be 0,1.
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let sep = "\n\n---\n\n";
        let page = prose(3);
        let junk = "a".repeat(200);
        let md = format!("{page}{sep}{junk}{sep}{page}");
        store.seed(COLLECTION, "doc", 6);

        let n = index(&store, &cfg, "doc", &md).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.stored(COLLECTION, "doc"), range(2));
        let st = store.state.lock();
        assert!(st.written.iter().all(|c| c.total_chunks == 2));
        assert!(st.written.iter().all(|c| c.raw_text != junk));
    }

    #[tokio::test]
    async fn short_form_collections_keep_short_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let n = index_with(
            &store,
            &cfg,
            "doc",
            "Tiny Title\n\nShort abstract.",
            ContentProfile::ShortFormCurated,
            "paper_abstracts",
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.stored("paper_abstracts", "doc"), [0]);
    }

    #[tokio::test]
    async fn one_write_per_document_carries_each_chunk_with_its_header() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let n = index(&store, &cfg, "doc", &prose(12)).await.unwrap();
        let st = store.state.lock();
        assert_eq!(st.write_calls, 1, "one write call per document");
        assert_eq!(st.written.len(), n as usize);
        for (i, c) in st.written.iter().enumerate() {
            assert!(c.text.starts_with(&format!("doc > chunk {i}\n\n")), "{c:?}");
            assert!(c.text.ends_with(&c.raw_text), "{c:?}");
            assert!(!c.raw_text.is_empty());
        }
    }

    #[tokio::test]
    async fn only_the_supplied_content_is_indexed_never_a_file_at_the_path() {
        // RA-3: the path names a real file with different text; the indexed
        // chunks must come from `content` alone.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret.md");
        std::fs::write(&file, "TOP-SECRET-FILE-CONTENT ".repeat(50)).unwrap();

        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        index_document(
            IndexJob {
                doc_id: "secret",
                markdown_path: file.to_str().unwrap(),
                content: &prose(6),
                catalog: None,
                collection: COLLECTION,
                profile: ContentProfile::FullDocument,
            },
            &cfg,
            &store,
            |_| {},
        )
        .await
        .unwrap();

        let st = store.state.lock();
        assert!(!st.written.is_empty());
        assert!(st.written.iter().all(|c| !c.text.contains("TOP-SECRET")));
    }

    #[tokio::test]
    async fn catalog_metadata_and_pages_reach_the_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let catalog = CatalogEntry {
            title: Some("A Real Title".into()),
            doi: Some("10.1/x".into()),
            publication_date: Some("2021-03-04".into()),
            ..Default::default()
        };
        index_document(
            IndexJob {
                doc_id: "doc",
                markdown_path: "markdown/do/doc.md",
                content: &prose(6),
                catalog: Some(catalog),
                collection: COLLECTION,
                profile: ContentProfile::FullDocument,
            },
            &cfg,
            &store,
            |_| {},
        )
        .await
        .unwrap();
        let st = store.state.lock();
        let chunk = &st.written[0];
        assert_eq!(chunk.meta.title.as_deref(), Some("A Real Title"));
        assert_eq!(chunk.meta.doi.as_deref(), Some("10.1/x"));
        assert_eq!(chunk.meta.markdown_path, "markdown/do/doc.md");
        assert!(chunk.text.starts_with("A Real Title > chunk 0"));
    }

    #[tokio::test]
    async fn an_invalid_chunker_config_is_an_error_not_a_hang() {
        let (store, mut cfg) = (FakeStore::default(), small_chunk_config());
        cfg.chunk_max_tokens = 0;
        let err = index(&store, &cfg, "doc", &prose(6)).await.unwrap_err();
        assert!(matches!(err, DistillError::Config(_)));
    }

    #[tokio::test]
    async fn llm_metadata_failure_fails_the_document() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Json(200, r#"{"response":"no json here"}"#.into())).await;
        let (store, mut cfg) = (FakeStore::default(), small_chunk_config());
        cfg.llm_metadata = true;
        cfg.ollama_url = fake.url();
        store.seed(COLLECTION, "doc", 3);

        let err = index(&store, &cfg, "doc", &prose(6)).await.unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
        assert!(store.state.lock().written.is_empty());
        assert_eq!(store.stored(COLLECTION, "doc"), range(3));
    }

    #[tokio::test]
    async fn llm_metadata_lands_on_the_chunks_when_the_model_answers() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| {
            Reply::Json(
                200,
                serde_json::json!({"response": r#"{"keywords":["k"],"topics":["t"]}"#}).to_string(),
            )
        })
        .await;
        let (store, mut cfg) = (FakeStore::default(), small_chunk_config());
        cfg.llm_metadata = true;
        cfg.ollama_url = fake.url();

        index(&store, &cfg, "doc", &prose(6)).await.unwrap();
        let st = store.state.lock();
        assert_eq!(st.written[0].meta.keywords, ["k"]);
        assert_eq!(st.written[0].meta.topics, ["t"]);
    }
}
