//! The shared indexability floor.
//!
//! One definition of "does this text carry enough content to be worth
//! embedding", used by both ends of the pipeline:
//!
//! - **Producer** (`hs-scribe`): rejects a conversion whose markdown falls
//!   below the floor instead of writing an empty `.md` and stamping a
//!   `conversion` success row that distill will only skip later.
//! - **Consumer** (`hs-distill`): drops individual chunks below the floor
//!   via `quality::explain`'s `TooShort` rule.
//!
//! Keeping the constant here is what makes the two agree. When the floor
//! lived only in `hs-distill`, scribe had no way to know that a 17-byte
//! `# 302 Found` stub could never produce a chunk, so it recorded the
//! conversion as successful and the junk only surfaced downstream as
//! `embedding_skip: zero_chunks_or_empty`.

/// Minimum non-whitespace character count for text to be worth embedding.
///
/// Applied per-chunk by `hs-distill`'s quality filter and per-document by
/// `hs-scribe`'s post-conversion gate. A document under this floor cannot
/// produce a single indexable chunk, because every chunk is a substring of
/// the document — which is what makes the document-level check sound rather
/// than merely heuristic.
pub const MIN_INDEXABLE_NON_WS: usize = 50;

/// Count non-whitespace characters in `text`.
pub fn non_whitespace_len(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// True when `text` clears [`MIN_INDEXABLE_NON_WS`].
///
/// Deliberately only the length floor — the richer per-chunk rules
/// (dominant character, n-gram repetition, token diversity) can legitimately
/// reject one chunk of an otherwise good document, so applying them to a
/// whole document would discard real papers. Length is monotone under
/// substring, so it is the only rule that transfers soundly from chunk to
/// document.
pub fn has_indexable_content(text: &str) -> bool {
    non_whitespace_len(text) >= MIN_INDEXABLE_NON_WS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_302_interstitial_stub() {
        // Verbatim markdown the html-parser produced for
        // 10.1088_0031-9155_58_11_R37 from a Radware 302 anti-bot page.
        // Scribe stamped this as a successful conversion before the floor
        // was shared; distill then skipped it as zero_chunks_or_empty.
        assert!(!has_indexable_content("# 302 Found\n\nrdwr"));
    }

    #[test]
    fn rejects_whitespace_only_and_empty() {
        assert!(!has_indexable_content(""));
        assert!(!has_indexable_content("   \n\n\t  \n"));
    }

    #[test]
    fn accepts_real_prose_at_the_boundary() {
        // Exactly at the floor: 50 non-whitespace chars must pass, 49 must not.
        let at_floor = "a".repeat(MIN_INDEXABLE_NON_WS);
        let below = "a".repeat(MIN_INDEXABLE_NON_WS - 1);
        assert!(has_indexable_content(&at_floor));
        assert!(!has_indexable_content(&below));
    }

    #[test]
    fn whitespace_does_not_count_toward_the_floor() {
        // Padding a short stub with newlines must not lift it over the bar.
        let padded = format!("# 302 Found\n\nrdwr{}", "\n".repeat(200));
        assert!(!has_indexable_content(&padded));
    }
}
