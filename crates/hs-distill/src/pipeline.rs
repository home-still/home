use hs_common::catalog::{CatalogEntry, PageOffset};
use hs_common::stores::{DerivedText, StoreKind};

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::client::DistillProgress;
use crate::config::DistillServerConfig;
use crate::error::DistillError;
use crate::metadata::extract_rule_based;
use crate::reference_list;
use crate::store::DocumentStore;
use crate::types::Chunk;

/// What kind of content a store deliberately carries: decides which
/// ingress quality gates `index_document` applies. The mapping lives in
/// exactly one place ([`ContentProfile::for_kind`]); a new kind of store
/// is given its gates THERE, not by name-compares scattered through the
/// code after its content silently drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentProfile {
    /// Full-body converted documents: gate on the paywall/interstitial
    /// stub heuristics and drop low-quality chunks.
    FullDocument,
    /// Deliberately short payloads (paper abstracts). The "short page
    /// without article structure = junk" and 50-char `TooShort` rules
    /// would reject exactly the content these stores exist to carry,
    /// so both gates are off: the producing pipeline already decided
    /// what is worth indexing.
    ShortFormCurated,
}

impl ContentProfile {
    /// The gates for the documents a store of this kind is sent. `None`
    /// for a kind that is sent none: a reference-list store is filled
    /// from its parent's documents, and a memory store holds statements,
    /// not documents.
    pub fn for_kind(kind: &StoreKind) -> Option<Self> {
        match kind {
            StoreKind::Documents(_) => Some(Self::FullDocument),
            StoreKind::Derived {
                text: DerivedText::CatalogAbstract,
                ..
            } => Some(Self::ShortFormCurated),
            StoreKind::Derived {
                text: DerivedText::ReferenceChunks,
                ..
            }
            | StoreKind::Memory => None,
        }
    }
}

/// One document to index. The text is always supplied by the caller; this
/// module never reads documents from disk.
pub struct IndexJob<'a> {
    /// Identity of the document in its store (the catalog stem).
    pub doc_id: &'a str,
    pub content: &'a str,
    /// The document's catalog entry, if the caller has one. It is the only
    /// source of title/authors/DOI/year: the server has no catalog of its
    /// own.
    pub catalog: Option<CatalogEntry>,
    /// The store the document is indexed into.
    pub store: &'a str,
    /// The store that takes the document's reference-list chunks, when
    /// `store` has one. Without one every chunk goes to `store`.
    pub references: Option<&'a str>,
    pub profile: ContentProfile,
}

/// Index a single markdown document: chunk -> metadata -> write.
///
/// After this returns `Ok(n)` the store, together with its reference
/// store, holds exactly the `n` chunks produced from `job.content` for
/// `doc_id`, and none left over from an earlier version of the document:
///
/// * `n > 0`: the new generation is written first, and only after every
///   write succeeded are the records of every earlier generation deleted
///   (all records of the document except the ones just written). A failed
///   write therefore never leaves a document with fewer chunks than before,
///   and a stale generation never outlives a re-index. Records a failed
///   write did leave behind stay until the next successful index of the
///   document removes them, and the caller records no success, so the
///   document is retried.
/// * `n == 0` (empty document, interstitial stub, no chunk passed the
///   quality filter): every earlier record of the document is deleted before
///   `Ok(0)` is returned, so a caller that stamps the document as skipped
///   is not contradicted by searchable chunks.
///
/// With a reference store, the chunks that are reference lists
/// ([`reference_list::is_reference_list`]) are written there and the rest
/// to `job.store`. A citation of work about a topic matches a query about
/// that topic as well as prose does, so kept beside the prose it outranks
/// the passage that answers the question; in a store of its own it is
/// found only by a search that names that store. `chunk_index` and
/// `total_chunks` count the whole document across both stores, and `n` is
/// that total.
///
/// Any failure, including the deletes, is an `Err` and changes nothing
/// about what the caller should record.
pub async fn index_document(
    job: IndexJob<'_>,
    config: &DistillServerConfig,
    store: &dyn DocumentStore,
    on_progress: impl Fn(DistillProgress),
) -> Result<u32, DistillError> {
    let IndexJob {
        doc_id: stem,
        content: markdown,
        catalog,
        store: store_name,
        references,
        profile,
    } = job;
    // Every store that can hold a chunk of this document.
    let targets = [Some(store_name), references];

    on_progress(DistillProgress {
        stage: "reading".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: 0,
        message: format!("Reading {stem}"),
    });

    if markdown.trim().is_empty() {
        tracing::warn!("Skipping empty document: {}", stem);
        remove_document(store, targets, stem).await?;
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
        remove_document(store, targets, stem).await?;
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

    let mut meta = extract_rule_based(markdown, stem, catalog.as_ref());

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
    // Short-form stores bypass the filter: their single-chunk
    // payloads fail the 50-char `TooShort` rule by design, and dropping
    // records behind the producer's back was causing 2,268 catalog stamps
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
        remove_document(store, targets, stem).await?;
        return Ok(0);
    }

    // Surviving chunks are renumbered so indices are contiguous 0..n: the
    // quality filter can drop chunks from the middle.
    let total_chunks = chunks.len() as u32;
    for (i, chunk) in chunks.iter_mut().enumerate() {
        chunk.chunk_index = i as u32;
        chunk.total_chunks = total_chunks;
    }

    let (reference_chunks, prose_chunks): (Vec<Chunk>, Vec<Chunk>) = match references {
        Some(_) => chunks
            .into_iter()
            .partition(|chunk| reference_list::is_reference_list(&chunk.raw_text)),
        None => (Vec::new(), chunks),
    };

    // Write. The store embeds each chunk (header included) and keeps the
    // text verbatim.
    on_progress(DistillProgress {
        stage: "writing".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: total_chunks as u64,
        message: format!("Writing {} chunks", total_chunks),
    });

    // (store, the records of this generation in it). A store that gets no
    // chunk of this generation is written nothing and keeps nothing.
    let mut generation: Vec<(&str, Vec<String>)> = Vec::with_capacity(targets.len());
    for (target, part) in targets.into_iter().zip([&prose_chunks, &reference_chunks]) {
        let Some(target) = target else { continue };
        let record_ids = if part.is_empty() {
            Vec::new()
        } else {
            store.write_document(target, stem, part).await?
        };
        generation.push((target, record_ids));
    }

    // Only now that every new chunk is stored: drop what earlier generations
    // of this document left, in both stores.
    for (target, record_ids) in &generation {
        store.forget_document(target, stem, record_ids).await?;
    }

    on_progress(DistillProgress {
        stage: "done".into(),
        doc: stem.to_string(),
        chunks_done: total_chunks as u64,
        chunks_total: total_chunks as u64,
        message: format!("Indexed {} chunks", total_chunks),
    });

    Ok(total_chunks)
}

/// Remove every stored chunk of a document that is being skipped, from
/// every store that can hold one.
async fn remove_document(
    store: &dyn DocumentStore,
    targets: [Option<&str>; 2],
    doc_id: &str,
) -> Result<(), DistillError> {
    for target in targets.into_iter().flatten() {
        store.forget_document(target, doc_id, &[]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{prose, small_chunk_config, FakeStore, Op, BIBLIOGRAPHY};
    use hs_common::markdown::PAGE_SEPARATOR;
    use hs_common::stores::{ABSTRACTS, ACADEMIC as STORE, REFERENCES};

    /// Index into a store that has no reference store.
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
            STORE,
            None,
        )
        .await
    }

    /// Index into a store whose reference lists go to [`REFERENCES`].
    async fn index_beside_references(
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
            STORE,
            Some(REFERENCES),
        )
        .await
    }

    async fn index_with(
        store: &FakeStore,
        config: &DistillServerConfig,
        doc_id: &str,
        content: &str,
        profile: ContentProfile,
        store_name: &str,
        references: Option<&str>,
    ) -> Result<u32, DistillError> {
        index_document(
            IndexJob {
                doc_id,
                content,
                catalog: None,
                store: store_name,
                references,
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

    /// Three pages, one chunk each: prose, a reference list, prose.
    fn paper_with_a_reference_list() -> String {
        let page = prose(3);
        format!("{page}{PAGE_SEPARATOR}{BIBLIOGRAPHY}{PAGE_SEPARATOR}{page}")
    }

    #[tokio::test]
    async fn reindexing_with_fewer_chunks_leaves_no_stale_generation() {
        // RA-42: the old code upserted `doc:0..n` and never deleted `doc:n..`.
        let (store, cfg) = (FakeStore::default(), small_chunk_config());

        let long = index(&store, &cfg, "doc", &prose(30)).await.unwrap();
        assert!(long >= 6, "need a multi-chunk document, got {long}");
        assert_eq!(store.stored(STORE, "doc"), range(long));

        let short = index(&store, &cfg, "doc", &prose(8)).await.unwrap();
        assert!(short < long);
        assert_eq!(
            store.stored(STORE, "doc"),
            range(short),
            "records of the earlier generation must be gone"
        );
    }

    #[tokio::test]
    async fn the_old_generation_is_deleted_only_after_the_write_succeeded_and_never_the_new_one() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(STORE, "doc", 20);

        let n = index(&store, &cfg, "doc", &prose(12)).await.unwrap();
        assert!(n > 2);

        let ops = store.ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        let Op::Write { indexes, .. } = &ops[0] else {
            panic!("the write comes first: {ops:?}");
        };
        assert_eq!(*indexes, range(n));
        let Op::Forget {
            store: forgotten_in,
            doc_id,
            keep,
        } = &ops[1]
        else {
            panic!("the cleanup comes last: {ops:?}");
        };
        assert_eq!((forgotten_in.as_str(), doc_id.as_str()), (STORE, "doc"));
        assert_eq!(keep.len(), n as usize, "keep exactly the new records");
        assert_eq!(store.stored(STORE, "doc"), range(n));
    }

    #[tokio::test]
    async fn a_failed_write_never_deletes_the_previous_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(STORE, "doc", 10);

        store.state.lock().fail_write_from_call = Some(0);
        let err = index(&store, &cfg, "doc", &prose(12)).await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)));

        assert!(
            !store.ops().iter().any(|op| matches!(op, Op::Forget { .. })),
            "no delete may run when the write failed: {:?}",
            store.ops()
        );
        assert_eq!(
            store.stored(STORE, "doc"),
            range(10),
            "the document must not lose chunks because its re-index failed"
        );
    }

    #[tokio::test]
    async fn a_failed_cleanup_after_a_good_write_is_an_error_not_a_success() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(STORE, "doc", 3);
        store.state.lock().fail_forget = true;
        let err = index(&store, &cfg, "doc", &prose(12)).await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)), "{err}");
    }

    /// Content `index_document` skips, with the reason it does.
    fn skipped_documents() -> [(&'static str, &'static str); 4] {
        let stub = "Checking your browser before accessing the site. Please wait.";
        assert!(
            hs_common::html::is_known_interstitial(stub),
            "fixture must be an interstitial"
        );
        [
            ("empty", ""),
            ("whitespace", "  \n\t \n"),
            ("interstitial", stub),
            ("no chunk passes the quality filter", "tiny"),
        ]
    }

    #[tokio::test]
    async fn every_skip_path_removes_the_documents_old_chunks() {
        // RA-42: each `Ok(0)` used to leave earlier chunks searchable while
        // the caller stamped the document as skipped.
        let cfg = small_chunk_config();
        for (label, content) in skipped_documents() {
            let store = FakeStore::default();
            store.seed(STORE, "doc", 7);
            let n = index(&store, &cfg, "doc", content).await.unwrap();
            assert_eq!(n, 0, "{label}");
            assert!(
                store.stored(STORE, "doc").is_empty(),
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
        store.seed(STORE, "keep", 4);
        store.seed(STORE, "skip", 4);
        index(&store, &cfg, "skip", "").await.unwrap();
        assert_eq!(store.stored(STORE, "keep"), range(4));
        assert!(store.stored(STORE, "skip").is_empty());
    }

    #[tokio::test]
    async fn a_skip_whose_cleanup_fails_is_an_error_not_a_recorded_skip() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(STORE, "doc", 3);
        store.state.lock().fail_forget = true;
        let err = index(&store, &cfg, "doc", "").await.unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)));
    }

    #[tokio::test]
    async fn chunk_indexes_stay_contiguous_when_the_quality_filter_drops_a_middle_chunk() {
        // Three pages -> three chunks; the middle one is a repetition loop.
        // The survivors must be 0,1.
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let page = prose(3);
        let junk = "a".repeat(200);
        let md = format!("{page}{PAGE_SEPARATOR}{junk}{PAGE_SEPARATOR}{page}");
        store.seed(STORE, "doc", 6);

        let n = index(&store, &cfg, "doc", &md).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.stored(STORE, "doc"), range(2));
        let st = store.state.lock();
        assert!(st.written.iter().all(|c| c.total_chunks == 2));
        assert!(st.written.iter().all(|c| c.raw_text != junk));
    }

    #[tokio::test]
    async fn a_short_form_store_keeps_short_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let n = index_with(
            &store,
            &cfg,
            "doc",
            "Tiny Title\n\nShort abstract.",
            ContentProfile::ShortFormCurated,
            ABSTRACTS,
            None,
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.stored(ABSTRACTS, "doc"), [0]);
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

    // ── reference lists ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_reference_list_goes_to_the_reference_store_and_the_prose_stays() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let n = index_beside_references(&store, &cfg, "doc", &paper_with_a_reference_list())
            .await
            .unwrap();

        assert_eq!(n, 3, "the count is the whole document, across both stores");
        assert_eq!(store.stored(STORE, "doc"), [0, 2]);
        assert_eq!(store.stored(REFERENCES, "doc"), [1]);
        let st = store.state.lock();
        assert!(
            st.written.iter().all(|c| c.total_chunks == 3),
            "every chunk is numbered within the whole document"
        );
        let moved: Vec<&str> = st
            .written
            .iter()
            .filter(|c| c.chunk_index == 1)
            .map(|c| c.raw_text.as_str())
            .collect();
        assert_eq!(moved, [BIBLIOGRAPHY]);
    }

    #[tokio::test]
    async fn without_a_reference_store_a_reference_list_stays_with_the_prose() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let n = index(&store, &cfg, "doc", &paper_with_a_reference_list())
            .await
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(store.stored(STORE, "doc"), range(3));
        assert!(store.stored(REFERENCES, "doc").is_empty());
        let touched: Vec<Op> = store
            .ops()
            .into_iter()
            .filter(|op| {
                let (Op::Write { store, .. } | Op::Forget { store, .. }) = op;
                store == REFERENCES
            })
            .collect();
        assert_eq!(touched, [], "a store that was not named was touched");
    }

    #[tokio::test]
    async fn both_stores_are_written_before_either_old_generation_is_deleted() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        store.seed(STORE, "doc", 5);
        store.seed(REFERENCES, "doc", 4);

        index_beside_references(&store, &cfg, "doc", &paper_with_a_reference_list())
            .await
            .unwrap();

        let order: Vec<(&str, String, usize)> = store
            .ops()
            .into_iter()
            .map(|op| match op {
                Op::Write { store, indexes, .. } => ("write", store, indexes.len()),
                Op::Forget { store, keep, .. } => ("forget", store, keep.len()),
            })
            .collect();
        assert_eq!(
            order,
            [
                ("write", STORE.to_string(), 2),
                ("write", REFERENCES.to_string(), 1),
                ("forget", STORE.to_string(), 2),
                ("forget", REFERENCES.to_string(), 1),
            ]
        );
        assert_eq!(store.stored(STORE, "doc"), [0, 2]);
        assert_eq!(store.stored(REFERENCES, "doc"), [1]);
    }

    #[tokio::test]
    async fn a_store_that_gets_no_chunk_still_loses_its_previous_generation() {
        let cfg = small_chunk_config();

        // The new version has no reference list: the old one must go.
        let store = FakeStore::default();
        store.seed(REFERENCES, "doc", 4);
        let n = index_beside_references(&store, &cfg, "doc", &prose(3))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.stored(STORE, "doc"), [0]);
        assert!(store.stored(REFERENCES, "doc").is_empty());

        // The new version is nothing but a reference list: the old prose
        // must go.
        let store = FakeStore::default();
        store.seed(STORE, "doc", 5);
        let n = index_beside_references(&store, &cfg, "doc", BIBLIOGRAPHY)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(store.stored(STORE, "doc").is_empty());
        assert_eq!(store.stored(REFERENCES, "doc"), [0]);
        assert_eq!(store.state.lock().write_calls, 1, "an empty write was sent");
    }

    #[tokio::test]
    async fn a_failed_write_to_either_store_deletes_nothing() {
        let cfg = small_chunk_config();
        // The prose is written first, the reference list second.
        for failing_write in [0, 1] {
            let store = FakeStore::default();
            store.seed(STORE, "doc", 5);
            store.seed(REFERENCES, "doc", 4);
            store.state.lock().fail_write_from_call = Some(failing_write);

            let err = index_beside_references(&store, &cfg, "doc", &paper_with_a_reference_list())
                .await
                .unwrap_err();
            assert!(matches!(err, DistillError::Backend(_)), "{err}");
            assert!(
                !store.ops().iter().any(|op| matches!(op, Op::Forget { .. })),
                "write {failing_write} failed, yet a delete ran: {:?}",
                store.ops()
            );
            assert_eq!(
                store.stored(STORE, "doc")[..5],
                range(5),
                "write {failing_write}"
            );
            assert_eq!(
                store.stored(REFERENCES, "doc"),
                range(4),
                "write {failing_write}"
            );
        }
    }

    #[tokio::test]
    async fn every_skip_path_removes_old_chunks_from_the_reference_store_too() {
        let cfg = small_chunk_config();
        for (label, content) in skipped_documents() {
            let store = FakeStore::default();
            store.seed(STORE, "doc", 7);
            store.seed(REFERENCES, "doc", 2);
            let n = index_beside_references(&store, &cfg, "doc", content)
                .await
                .unwrap();
            assert_eq!(n, 0, "{label}");
            assert!(store.stored(STORE, "doc").is_empty(), "{label}");
            assert!(store.stored(REFERENCES, "doc").is_empty(), "{label}");
        }
    }

    #[tokio::test]
    async fn catalog_metadata_reaches_the_chunks() {
        let (store, cfg) = (FakeStore::default(), small_chunk_config());
        let catalog = CatalogEntry {
            title: Some("A Real Title".into()),
            doi: Some("10.1/x".into()),
            publication_date: Some("2021-03-04".into()),
            source_url: Some("https://example.org/paper".into()),
            ..Default::default()
        };
        index_document(
            IndexJob {
                doc_id: "doc",
                content: &prose(6),
                catalog: Some(catalog),
                store: STORE,
                references: None,
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
        assert_eq!(
            chunk.meta.source_url.as_deref(),
            Some("https://example.org/paper")
        );
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
        store.seed(STORE, "doc", 3);

        let err = index(&store, &cfg, "doc", &prose(6)).await.unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
        assert!(store.state.lock().written.is_empty());
        assert_eq!(store.stored(STORE, "doc"), range(3));
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
