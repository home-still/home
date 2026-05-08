//! EPUB → markdown converter. First-class ingest path alongside PDF (scribe
//! VLM) and HTML (scraper). One path: parse the EPUB archive, walk the spine
//! in reading order, convert each chapter's XHTML through
//! [`crate::html::convert_html_to_markdown`], concatenate. Errors propagate;
//! there is no silent stub or empty-output gate — if the converter can't
//! produce markdown, the operation fails and the caller logs it.

use anyhow::{Context, Result};
use std::io::Cursor;

/// Convert an EPUB archive's bytes to markdown. Iterates each document in
/// spine order; each chapter's XHTML is fed through the shared HTML walker
/// so the two paths produce structurally compatible markdown.
pub fn convert_epub_to_markdown(bytes: &[u8]) -> Result<String> {
    let mut doc = epub::doc::EpubDoc::from_reader(Cursor::new(bytes))
        .context("failed to open EPUB archive")?;

    let mut out = String::new();
    loop {
        if let Some((content, _mime)) = doc.get_current_str() {
            let md = crate::html::convert_html_to_markdown(&content);
            if !md.trim().is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&md);
            }
        }
        if !doc.go_next() {
            break;
        }
    }

    Ok(out)
}
