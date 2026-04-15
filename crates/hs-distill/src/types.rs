use serde::{Deserialize, Serialize};

/// Byte/line span in the source markdown file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkSpan {
    /// 1-based line number where the chunk starts.
    pub line_start: usize,
    /// 1-based line number where the chunk ends (inclusive).
    pub line_end: usize,
    /// Byte offset in the markdown where the chunk starts.
    pub char_start: usize,
    /// Byte offset in the markdown where the chunk ends.
    pub char_end: usize,
}

/// Metadata extracted from a document (catalog + regex + LLM).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentMeta {
    pub doc_id: String,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub doi: Option<String>,
    pub publication_date: Option<String>,
    pub abstract_text: Option<String>,
    pub cited_by_count: Option<u64>,
    pub source: Option<String>,
    pub pdf_path: Option<String>,
    pub markdown_path: String,
    pub keywords: Vec<String>,
    pub topics: Vec<String>,
}

/// A chunk of text with its position in the source markdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub doc_id: String,
    pub chunk_index: u32,
    pub total_chunks: u32,
    /// The chunk text with CCH header prepended for embedding.
    pub text: String,
    /// The raw chunk text without header (for display/search results).
    pub raw_text: String,
    pub span: ChunkSpan,
    pub page: Option<usize>,
    pub meta: DocumentMeta,
}

/// Dense + optional sparse embedding output.
#[derive(Debug, Clone)]
pub struct EmbeddingOutput {
    pub dense: Vec<f32>,
    pub sparse: Option<SparseVec>,
}

/// Sparse vector (index-value pairs).
#[derive(Debug, Clone)]
pub struct SparseVec {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

/// A chunk with its computed embedding, ready for Qdrant upsert.
#[derive(Debug, Clone)]
pub struct EmbeddedChunk {
    pub chunk: Chunk,
    pub embedding: EmbeddingOutput,
}
