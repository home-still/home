use crate::types::{Chunk, ChunkSpan, DocumentMeta};
use hs_common::catalog::PageOffset;

pub struct ChunkerConfig {
    pub max_tokens: usize,
    pub overlap_tokens: usize,
    /// Approximate characters per token for chunk sizing.
    pub chars_per_token: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1000,
            overlap_tokens: 100,
            chars_per_token: 4,
        }
    }
}

/// Build an index mapping each line number (0-based) to its byte offset in the text.
fn build_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Given a byte offset, return the 1-based line number via binary search.
fn byte_to_line(line_offsets: &[usize], byte_pos: usize) -> usize {
    match line_offsets.binary_search(&byte_pos) {
        Ok(idx) => idx + 1,
        Err(idx) => idx, // idx is the line that contains this byte
    }
}

/// Resolve which page a byte offset falls on, using catalog page offsets.
fn resolve_page(page_offsets: &[PageOffset], char_start: usize) -> Option<usize> {
    page_offsets
        .iter()
        .find(|po| char_start >= po.char_start && char_start < po.char_end)
        .map(|po| po.page)
}

const PAGE_SEPARATOR: &str = "\n\n---\n\n";

/// Split markdown into chunks with line-number tracking.
pub fn chunk_markdown(
    markdown: &str,
    doc_meta: &DocumentMeta,
    page_offsets: &[PageOffset],
    config: &ChunkerConfig,
) -> Vec<Chunk> {
    let line_offsets = build_line_offsets(markdown);
    let max_chars = config.max_tokens * config.chars_per_token;
    let overlap_chars = config.overlap_tokens * config.chars_per_token;

    // Split into segments at page separators first
    let segments = split_at_pages(markdown);

    let mut chunks = Vec::new();
    let mut global_offset: usize = 0;

    for segment in &segments {
        let segment_chunks = split_segment(segment, max_chars, overlap_chars);

        for chunk_text in segment_chunks {
            // Find byte position of this chunk within the full markdown
            let char_start = global_offset
                + markdown[global_offset..]
                    .find(chunk_text.as_str())
                    .unwrap_or(0);
            let char_end = char_start + chunk_text.len();

            let line_start = byte_to_line(&line_offsets, char_start);
            let line_end = byte_to_line(&line_offsets, char_end.saturating_sub(1)).max(line_start);

            let page = resolve_page(page_offsets, char_start);

            let span = ChunkSpan {
                line_start,
                line_end,
                char_start,
                char_end,
            };

            // CCH header for embedding quality
            let title = doc_meta.title.as_deref().unwrap_or(&doc_meta.doc_id);
            let text_with_header = format!("{} > chunk {}\n\n{}", title, chunks.len(), chunk_text);

            chunks.push(Chunk {
                doc_id: doc_meta.doc_id.clone(),
                chunk_index: 0, // set below
                total_chunks: 0,
                text: text_with_header,
                raw_text: chunk_text,
                span,
                page,
                meta: doc_meta.clone(),
            });
        }

        global_offset += segment.len() + PAGE_SEPARATOR.len();
    }

    // Set chunk indices
    let total = chunks.len() as u32;
    for (i, chunk) in chunks.iter_mut().enumerate() {
        chunk.chunk_index = i as u32;
        chunk.total_chunks = total;
    }

    chunks
}

/// Split markdown text at page separators, returning segments.
fn split_at_pages(text: &str) -> Vec<&str> {
    text.split(PAGE_SEPARATOR).collect()
}

/// Split a text segment into chunks respecting sentence boundaries.
fn split_segment(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        return vec![trimmed.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let end = (start + max_chars).min(text.len());

        // Look for sentence boundary going backwards from end
        let actual_end = if end < text.len() {
            find_sentence_boundary(text, start, end).unwrap_or(end)
        } else {
            end
        };

        let chunk_text = text[start..actual_end].trim();
        if !chunk_text.is_empty() {
            chunks.push(chunk_text.to_string());
        }

        // Advance with overlap
        let advance = actual_end.saturating_sub(overlap_chars);
        if advance <= start {
            start = actual_end; // force progress
        } else {
            start = advance;
        }
    }

    chunks
}

/// Find a sentence boundary (`. `, `? `, `! `, or `\n\n`) going backwards from `end`.
/// Searches back 20% of the chunk size.
fn find_sentence_boundary(text: &str, start: usize, end: usize) -> Option<usize> {
    let lookback = (end - start) / 5;
    let search_start = end.saturating_sub(lookback);

    let search_region = &text[search_start..end];

    // Prefer paragraph breaks
    if let Some(pos) = search_region.rfind("\n\n") {
        return Some(search_start + pos + 2);
    }

    // Then sentence endings
    for ending in &[". ", "? ", "! "] {
        if let Some(pos) = search_region.rfind(ending) {
            return Some(search_start + pos + ending.len());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta() -> DocumentMeta {
        DocumentMeta {
            doc_id: "test-doc".into(),
            title: Some("Test Document".into()),
            markdown_path: "markdown/test-doc.md".into(),
            ..Default::default()
        }
    }

    #[test]
    fn short_doc_single_chunk() {
        let md = "This is a short document.";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);
        assert_eq!(chunks[0].span.line_start, 1);
        assert_eq!(chunks[0].span.line_end, 1);
    }

    #[test]
    fn page_boundary_splits_chunks() {
        let md = "Page one content.\n\n---\n\nPage two content.";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default());
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].raw_text.contains("Page one"));
        assert!(chunks[1].raw_text.contains("Page two"));
    }

    #[test]
    fn line_numbers_correct() {
        let md = "Line 1\nLine 2\nLine 3\nLine 4";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].span.line_start, 1);
        assert_eq!(chunks[0].span.line_end, 4);
    }

    #[test]
    fn cch_header_prepended() {
        let md = "Some text here.";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default());
        assert!(chunks[0].text.starts_with("Test Document > chunk 0"));
    }

    #[test]
    fn empty_doc_no_chunks() {
        let md = "";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default());
        assert_eq!(chunks.len(), 0);
    }
}
