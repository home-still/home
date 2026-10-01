use std::ops::Range;

use crate::error::DistillError;
use crate::text::ceil_char_boundary;
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

impl ChunkerConfig {
    /// Reject sizes the splitter cannot make progress with: a zero chunk
    /// size (the old code spun forever on `max_tokens: 0`) or an overlap
    /// that is not smaller than the chunk.
    pub fn validate(&self) -> Result<(), DistillError> {
        if self.max_tokens == 0 {
            return Err(DistillError::Config("chunk_max_tokens must be > 0".into()));
        }
        if self.chars_per_token == 0 {
            return Err(DistillError::Config("chars_per_token must be > 0".into()));
        }
        if self.overlap_tokens >= self.max_tokens {
            return Err(DistillError::Config(format!(
                "chunk_overlap ({}) must be smaller than chunk_max_tokens ({})",
                self.overlap_tokens, self.max_tokens
            )));
        }
        Ok(())
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

/// Split markdown into chunks with line-number tracking. Every chunk's
/// `span` is its exact byte range in `markdown` (`markdown[char_start..
/// char_end] == raw_text`), carried from the splitter rather than searched
/// for, so repeated text cannot be mistaken for an earlier occurrence.
pub fn chunk_markdown(
    markdown: &str,
    doc_meta: &DocumentMeta,
    page_offsets: &[PageOffset],
    config: &ChunkerConfig,
) -> Result<Vec<Chunk>, DistillError> {
    config.validate()?;
    let line_offsets = build_line_offsets(markdown);
    let max_chars = config.max_tokens * config.chars_per_token;
    let overlap_chars = config.overlap_tokens * config.chars_per_token;

    // Split into segments at page separators first
    let segments = split_at_pages(markdown);

    let mut chunks = Vec::new();
    // Byte offset of the current segment inside `markdown`.
    let mut segment_offset: usize = 0;

    for segment in &segments {
        for range in split_segment(segment, max_chars, overlap_chars) {
            let char_start = segment_offset + range.start;
            let char_end = segment_offset + range.end;
            let chunk_text = segment[range].to_string();

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

        segment_offset += segment.len() + PAGE_SEPARATOR.len();
    }

    // Set chunk indices
    let total = chunks.len() as u32;
    for (i, chunk) in chunks.iter_mut().enumerate() {
        chunk.chunk_index = i as u32;
        chunk.total_chunks = total;
    }

    Ok(chunks)
}

/// Split markdown text at page separators, returning segments.
fn split_at_pages(text: &str) -> Vec<&str> {
    text.split(PAGE_SEPARATOR).collect()
}

/// `text[start..end]` with surrounding whitespace removed, as a byte range
/// of `text`; `None` when nothing but whitespace remains.
fn trimmed_range(text: &str, start: usize, end: usize) -> Option<Range<usize>> {
    let slice = &text[start..end];
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lead = slice.len() - slice.trim_start().len();
    Some(start + lead..start + lead + trimmed.len())
}

/// Split a text segment into chunks respecting sentence boundaries. Returns
/// each chunk as a trimmed, non-empty byte range of `text`. `max_chars` must
/// be non-zero (see [`ChunkerConfig::validate`]).
fn split_segment(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<Range<usize>> {
    if text.len() <= max_chars {
        return trimmed_range(text, 0, text.len()).into_iter().collect();
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let end = ceil_char_boundary(text, (start + max_chars).min(text.len()));

        // Look for sentence boundary going backwards from end
        let actual_end = if end < text.len() {
            find_sentence_boundary(text, start, end).unwrap_or(end)
        } else {
            end
        };

        if let Some(range) = trimmed_range(text, start, actual_end) {
            chunks.push(range);
        }

        // The chunk that reaches the end of the text is the last one.
        // Re-entering at `len - overlap` would emit a final chunk wholly
        // contained in this one: a duplicate vector per long segment.
        if actual_end >= text.len() {
            break;
        }

        // Advance with overlap
        let advance = ceil_char_boundary(text, actual_end.saturating_sub(overlap_chars));
        start = if advance <= start {
            actual_end // force progress
        } else {
            advance
        };
    }

    chunks
}

/// Find a sentence boundary (`. `, `? `, `! `, or `\n\n`) going backwards from `end`.
/// Searches back 20% of the chunk size.
fn find_sentence_boundary(text: &str, start: usize, end: usize) -> Option<usize> {
    let lookback = (end - start) / 5;
    let search_start = ceil_char_boundary(text, end.saturating_sub(lookback));
    let end = ceil_char_boundary(text, end);

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
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].total_chunks, 1);
        assert_eq!(chunks[0].span.line_start, 1);
        assert_eq!(chunks[0].span.line_end, 1);
    }

    #[test]
    fn page_boundary_splits_chunks() {
        let md = "Page one content.\n\n---\n\nPage two content.";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default()).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].raw_text.contains("Page one"));
        assert!(chunks[1].raw_text.contains("Page two"));
    }

    #[test]
    fn line_numbers_correct() {
        let md = "Line 1\nLine 2\nLine 3\nLine 4";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].span.line_start, 1);
        assert_eq!(chunks[0].span.line_end, 4);
    }

    #[test]
    fn cch_header_prepended() {
        let md = "Some text here.";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default()).unwrap();
        assert!(chunks[0].text.starts_with("Test Document > chunk 0"));
    }

    #[test]
    fn empty_doc_no_chunks() {
        let md = "";
        let chunks = chunk_markdown(md, &make_meta(), &[], &ChunkerConfig::default()).unwrap();
        assert_eq!(chunks.len(), 0);
    }

    // ── Multi-chunk behavior (RA-88) ───────────────────────────────────

    fn small_config() -> ChunkerConfig {
        // 200-char chunks, 40-char overlap.
        ChunkerConfig {
            max_tokens: 50,
            overlap_tokens: 10,
            chars_per_token: 4,
        }
    }

    fn run(md: &str, cfg: &ChunkerConfig) -> Vec<Chunk> {
        chunk_markdown(md, &make_meta(), &[], cfg).unwrap()
    }

    fn prose(sentences: usize) -> String {
        (0..sentences)
            .map(|i| format!("Sentence number {i} says something distinct about topic {i}."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn no_chunk_is_contained_in_the_previous_one() {
        for n in [5, 12, 30, 61] {
            let md = prose(n);
            let chunks = run(&md, &small_config());
            assert!(chunks.len() > 1, "need a multi-chunk document (n={n})");
            for pair in chunks.windows(2) {
                assert!(
                    pair[1].span.char_end > pair[0].span.char_end,
                    "n={n}: chunk {} ({}..{}) adds nothing beyond chunk {} ({}..{})",
                    pair[1].chunk_index,
                    pair[1].span.char_start,
                    pair[1].span.char_end,
                    pair[0].chunk_index,
                    pair[0].span.char_start,
                    pair[0].span.char_end,
                );
            }
        }
    }

    #[test]
    fn last_chunk_ends_at_the_end_of_the_document() {
        let md = prose(40);
        let chunks = run(&md, &small_config());
        assert_eq!(chunks.last().unwrap().span.char_end, md.trim_end().len());
    }

    #[test]
    fn spans_slice_back_to_the_chunk_text() {
        // A repeated sentence makes `find(chunk_text)` ambiguous: every
        // chunk would resolve to the first occurrence.
        let md = "The same sentence is repeated verbatim. ".repeat(40);
        let chunks = run(md.trim_end(), &small_config());
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert_eq!(
                &md[c.span.char_start..c.span.char_end],
                c.raw_text,
                "chunk {}",
                c.chunk_index
            );
        }
        let starts: Vec<_> = chunks.iter().map(|c| c.span.char_start).collect();
        assert!(
            starts.windows(2).all(|w| w[0] < w[1]),
            "chunk starts must strictly increase: {starts:?}"
        );
    }

    #[test]
    fn consecutive_chunks_overlap_by_about_the_configured_amount() {
        let md = prose(40);
        let cfg = small_config();
        let overlap_chars = cfg.overlap_tokens * cfg.chars_per_token;
        let chunks = run(&md, &cfg);
        for pair in chunks.windows(2) {
            let overlap = pair[0]
                .span
                .char_end
                .saturating_sub(pair[1].span.char_start);
            assert!(
                overlap > 0,
                "chunks {} and {} do not overlap",
                pair[0].chunk_index,
                pair[1].chunk_index
            );
            // Overlap is `overlap_chars` plus whitespace trimming and the
            // word/char-boundary snap.
            assert!(
                overlap <= overlap_chars + 8,
                "overlap {overlap} exceeds configured {overlap_chars}"
            );
        }
    }

    #[test]
    fn chunks_cover_the_whole_document() {
        let md = prose(50);
        let chunks = run(&md, &small_config());
        let mut covered_to = 0;
        for c in &chunks {
            assert!(
                c.span.char_start <= covered_to + 1,
                "gap before chunk {} (covered to {covered_to}, starts {})",
                c.chunk_index,
                c.span.char_start
            );
            covered_to = covered_to.max(c.span.char_end);
        }
        assert_eq!(covered_to, md.len());
    }

    #[test]
    fn non_ascii_text_chunks_on_char_boundaries() {
        let md = "数据分析的结果表明，模型在多种条件下都表现稳定。🙂 Ünïcödé façade — naïve café. "
            .repeat(30);
        let md = md.trim_end();
        let chunks = run(md, &small_config());
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert!(md.is_char_boundary(c.span.char_start));
            assert!(md.is_char_boundary(c.span.char_end));
            assert_eq!(&md[c.span.char_start..c.span.char_end], c.raw_text);
        }
        for pair in chunks.windows(2) {
            assert!(pair[1].span.char_end > pair[0].span.char_end);
        }
    }

    #[test]
    fn each_page_segment_is_chunked_with_global_offsets() {
        let page = prose(12);
        let md = format!("{page}{PAGE_SEPARATOR}{page}{PAGE_SEPARATOR}{page}");
        let chunks = run(&md, &small_config());
        for c in &chunks {
            assert_eq!(&md[c.span.char_start..c.span.char_end], c.raw_text);
        }
        let per_page = chunks.len() / 3;
        assert_eq!(chunks.len(), per_page * 3, "pages chunk identically");
        assert!(chunks[per_page].span.char_start >= page.len() + PAGE_SEPARATOR.len());
    }

    #[test]
    fn indices_and_totals_are_consistent() {
        let chunks = run(&prose(40), &small_config());
        let total = chunks.len() as u32;
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.chunk_index, i as u32);
            assert_eq!(c.total_chunks, total);
            assert!(c.text.contains(&c.raw_text));
        }
    }

    // ── Config validation (RA-88) ──────────────────────────────────────

    #[test]
    fn zero_chunk_size_is_rejected_instead_of_looping_forever() {
        let cfg = ChunkerConfig {
            max_tokens: 0,
            overlap_tokens: 0,
            chars_per_token: 4,
        };
        assert!(matches!(
            chunk_markdown(&prose(10), &make_meta(), &[], &cfg),
            Err(DistillError::Config(_))
        ));
        let cfg = ChunkerConfig {
            chars_per_token: 0,
            ..ChunkerConfig::default()
        };
        assert!(matches!(cfg.validate(), Err(DistillError::Config(_))));
    }

    #[test]
    fn overlap_must_be_smaller_than_the_chunk() {
        for (max_tokens, overlap_tokens) in [(50, 50), (50, 51), (1, 1)] {
            let cfg = ChunkerConfig {
                max_tokens,
                overlap_tokens,
                chars_per_token: 4,
            };
            assert!(
                matches!(cfg.validate(), Err(DistillError::Config(_))),
                "{max_tokens}/{overlap_tokens}"
            );
        }
        assert!(ChunkerConfig::default().validate().is_ok());
    }

    #[test]
    fn one_char_chunks_over_wide_characters_still_terminate() {
        let cfg = ChunkerConfig {
            max_tokens: 1,
            overlap_tokens: 0,
            chars_per_token: 1,
        };
        let md = "数".repeat(20);
        let chunks = chunk_markdown(&md, &make_meta(), &[], &cfg).unwrap();
        assert_eq!(chunks.len(), 20);
        for c in &chunks {
            assert_eq!(&md[c.span.char_start..c.span.char_end], c.raw_text);
        }
    }
}
