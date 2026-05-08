//! Lightweight client-side PDF metadata extraction.
//!
//! Used by the subscriber to size per-request convert timeouts against
//! PDF page count, so a 500-page book gets a generous deadline while
//! small papers don't sit idle on the floor. Depends on `lopdf` (pure
//! Rust, parses xref + trailer only — fast enough to run on every
//! dispatch).

/// Returns the number of pages in `bytes` or `None` if the input isn't
/// a parseable PDF. Caller treats `None` as "use the config fallback
/// timeout" — we don't want a malformed xref to block the queue.
pub fn count_pages(bytes: &[u8]) -> Option<u32> {
    let doc = lopdf::Document::load_mem(bytes).ok()?;
    let count = doc.get_pages().len();
    u32::try_from(count).ok()
}

#[cfg(test)]
mod tests {
    // Positive-case parsing is covered by lopdf's own test suite;
    // here we only verify our wrapper degrades gracefully on non-PDF
    // inputs, which is what the subscriber relies on to fall back to
    // `TimeoutPolicy::fallback_secs`.
    use super::*;

    #[test]
    fn returns_none_on_garbage() {
        assert_eq!(count_pages(b"not a pdf at all"), None);
    }

    #[test]
    fn returns_none_on_empty() {
        assert_eq!(count_pages(b""), None);
    }

    #[test]
    fn returns_none_on_truncated_pdf_header() {
        assert_eq!(count_pages(b"%PDF-1.4\n"), None);
    }
}
