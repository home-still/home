use crate::error::{PersonalError, Result};

/// EPUB → markdown via the existing `hs_scribe::epub` library. Reuses the same
/// chapter walker as the academic pipeline so personal EPUBs produce
/// structurally compatible markdown.
pub fn convert(bytes: Vec<u8>) -> Result<String> {
    hs_scribe::epub::convert_epub_to_markdown(&bytes).map_err(|e| PersonalError::Converter {
        format: "epub",
        source: e,
    })
}
