use std::path::Path;

use hs_common::catalog::{read_catalog_entry, PageOffset};

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::client::DistillProgress;
use crate::config::DistillServerConfig;
use crate::embed::Embedder;
use crate::error::DistillError;
use crate::metadata::extract_rule_based;
use crate::qdrant;
use crate::types::EmbeddedChunk;

/// Index a single markdown document: chunk -> metadata -> embed -> upsert.
/// If `content` is provided, uses it directly instead of reading from disk.
pub async fn index_document(
    markdown_path: &Path,
    content: Option<&str>,
    config: &DistillServerConfig,
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

    // Read catalog entry if available
    let catalog_dir = markdown_path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("catalog"))
        .unwrap_or_default();

    let catalog_entry = read_catalog_entry(&catalog_dir, stem);
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

    // Upsert in batches of 500
    for batch in embedded_chunks.chunks(500) {
        qdrant::upsert_chunks(qdrant_client, &config.collection_name, batch).await?;
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
