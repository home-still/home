use std::path::Path;

use futures_util::stream::{FuturesUnordered, StreamExt};
use hs_common::catalog::{read_catalog_entry, PageOffset};

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::client::DistillProgress;
use crate::config::DistillServerConfig;
use crate::embed::Embedder;
use crate::error::DistillError;
use crate::metadata::extract_rule_based;
use crate::qdrant;
use crate::types::EmbeddedChunk;

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
    /// Deliberately short, degraded-signal payloads (abstracts,
    /// title-only stems). The "short page without article structure =
    /// junk" and 50-char `TooShort` rules would reject exactly the
    /// content these collections exist to carry, so both gates are off
    /// — the producing pipeline already made the informed decision to
    /// index degraded signal.
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

/// Index a single markdown document: chunk -> metadata -> embed -> upsert.
/// If `content` is provided, uses it directly instead of reading from disk.
/// If `catalog_override` is provided, uses it directly and skips the
/// filesystem-based catalog lookup — the canonical way for clients that
/// already have the catalog entry (e.g. hs-mcp) to pass it in rather than
/// relying on the distill server's local filesystem layout.
#[allow(clippy::too_many_arguments)]
pub async fn index_document(
    markdown_path: &Path,
    content: Option<&str>,
    catalog_override: Option<hs_common::catalog::CatalogEntry>,
    config: &DistillServerConfig,
    collection_name: &str,
    profile: ContentProfile,
    embedder: &dyn Embedder,
    qdrant_client: &qdrant_client::Qdrant,
    on_progress: impl Fn(DistillProgress),
) -> Result<u32, DistillError> {
    let stem = markdown_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let markdown_path_str = markdown_path.to_string_lossy().to_string();

    on_progress(DistillProgress {
        stage: "reading".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: 0,
        message: format!("Reading {stem}"),
    });

    // Use provided content or read from disk
    let markdown = match content {
        Some(c) => c.to_string(),
        None => std::fs::read_to_string(markdown_path).map_err(DistillError::Io)?,
    };

    if markdown.trim().is_empty() {
        tracing::warn!("Skipping empty document: {}", stem);
        return Ok(0);
    }

    // Belt-and-suspenders: refuse markdown that looks like a paywall /
    // anti-bot interstitial / stub even though the downloader and scribe
    // already gate at ingress. A new origin-side anti-bot variant or a
    // pre-rc.315 residual stub would otherwise be embedded as a 1-chunk
    // vector and poison search. Returning Ok(0) routes through
    // record_embedding_outcome_via → embedding_skip = zero_chunks_or_empty,
    // which the reconciler treats as an intentional terminal skip.
    //
    // Short-form collections deliberately ship content that trips the
    // "short page without article structure = junk" rule inside
    // `is_paywall_html` — see ContentProfile.
    if profile == ContentProfile::FullDocument && hs_common::html::is_paywall_html(&markdown) {
        tracing::warn!(
            stem,
            len = markdown.len(),
            "Skipping paywall/interstitial markdown stub"
        );
        return Ok(0);
    }

    // Prefer a caller-supplied catalog entry (e.g. hs-mcp loads it via
    // Storage and passes it through the HTTP request). Fall back to the
    // filesystem walk only when no override was provided — that path is
    // still used by the CLI batch flow where markdown and catalog live
    // as sibling directories.
    // With sharded layout: markdown/XX/stem.md → markdown/XX/ → markdown/ → project_root/
    let catalog_entry = match catalog_override {
        Some(cat) => Some(cat),
        None => {
            let catalog_dir = markdown_path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.join("catalog"))
                .unwrap_or_default();
            read_catalog_entry(&catalog_dir, stem)
        }
    };
    let page_offsets: Vec<PageOffset> = catalog_entry
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

    let mut meta = extract_rule_based(&markdown, stem, &markdown_path_str, catalog_entry.as_ref());
    // pdf_path is always populated as a sharded storage key by
    // `extract_rule_based`; no host-filesystem fallback.

    // Optional LLM metadata extraction
    if config.llm_metadata {
        match crate::metadata::extract_llm_metadata(
            &markdown,
            &config.ollama_url,
            &config.metadata_model,
        )
        .await
        {
            Ok((keywords, topics)) => {
                meta.keywords = keywords;
                meta.topics = topics;
            }
            Err(e) => {
                tracing::warn!("LLM metadata extraction failed: {e}");
            }
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

    let chunks = chunk_markdown(&markdown, &meta, &page_offsets, &chunker_config);

    // Filter out low-quality chunks (repetition loops, garbled text, etc.)
    // Short-form collections bypass the filter — their single-chunk
    // payloads fail the 50-char `TooShort` rule by design, and dropping
    // points behind the producer's back was causing 2,268 catalog stamps
    // to point at non-existent Qdrant rows. See ContentProfile.
    let chunks: Vec<_> = if profile == ContentProfile::ShortFormCurated {
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

    let total_chunks = chunks.len() as u32;

    if chunks.is_empty() {
        tracing::warn!("No chunks produced for {}", stem);
        return Ok(0);
    }

    // Embed
    on_progress(DistillProgress {
        stage: "embedding".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: total_chunks as u64,
        message: format!("Embedding {} chunks", total_chunks),
    });

    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let embeddings = embedder.embed_batch(&texts).await?;

    let embedded_chunks: Vec<EmbeddedChunk> = chunks
        .into_iter()
        .zip(embeddings)
        .map(|(chunk, embedding)| EmbeddedChunk { chunk, embedding })
        .collect();

    // Upsert to Qdrant
    on_progress(DistillProgress {
        stage: "upserting".into(),
        doc: stem.to_string(),
        chunks_done: 0,
        chunks_total: total_chunks as u64,
        message: format!("Upserting {} chunks to Qdrant", total_chunks),
    });

    // Upsert in config-sized batches, several in flight at once — Qdrant
    // handles concurrent writes to one collection cheaply, and the old
    // sequential loop became the slow link once embed got faster.
    let upsert_batch = config.qdrant_upsert_batch.max(1);
    let parallelism = config.qdrant_upsert_parallelism.max(1);
    let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
    for batch in embedded_chunks.chunks(upsert_batch) {
        in_flight.push(qdrant::upsert_chunks(qdrant_client, collection_name, batch));
        if in_flight.len() >= parallelism {
            if let Some(r) = in_flight.next().await {
                r?;
            }
        }
    }
    while let Some(r) = in_flight.next().await {
        r?;
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
