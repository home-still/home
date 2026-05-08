//! Format → markdown dispatch. Each branch routes to exactly one converter:
//! no fallbacks, no "try X then Y". Unknown extension is a hard error so that
//! adding a format is an explicit code change, not a silent decision at runtime.

pub mod docx;
pub mod epub;
pub mod passthrough;
pub mod pdf;

use crate::config::Config;
use crate::error::{PersonalError, Result};
use crate::models::SourceFormat;
use std::path::Path;

/// Convert the bytes of a source file to markdown. The caller has already
/// decided the format; this entry point is the single fan-in for the ingest
/// pipeline.
pub async fn convert(
    cfg: &Config,
    format: SourceFormat,
    bytes: Vec<u8>,
    stem_hint: &str,
) -> Result<String> {
    match format {
        SourceFormat::Pdf => pdf::convert(cfg, bytes, stem_hint).await,
        SourceFormat::Epub => epub::convert(bytes),
        SourceFormat::Docx => docx::convert(bytes),
        SourceFormat::Markdown => passthrough::convert_markdown(bytes),
        SourceFormat::Txt => passthrough::convert_txt(bytes),
    }
}

/// Resolve a file path to a `SourceFormat`. Unknown extension = hard error.
pub fn detect_format(path: &Path) -> Result<SourceFormat> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| PersonalError::UnsupportedExtension(path.display().to_string()))?;
    SourceFormat::from_extension(ext)
        .ok_or_else(|| PersonalError::UnsupportedExtension(ext.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_format_routes_known_extensions() {
        assert_eq!(
            detect_format(&PathBuf::from("foo.pdf")).unwrap(),
            SourceFormat::Pdf
        );
        assert_eq!(
            detect_format(&PathBuf::from("foo.epub")).unwrap(),
            SourceFormat::Epub
        );
        assert_eq!(
            detect_format(&PathBuf::from("nested/dir/foo.docx")).unwrap(),
            SourceFormat::Docx
        );
        assert_eq!(
            detect_format(&PathBuf::from("foo.MD")).unwrap(),
            SourceFormat::Markdown
        );
        assert_eq!(
            detect_format(&PathBuf::from("notes.txt")).unwrap(),
            SourceFormat::Txt
        );
    }

    #[test]
    fn detect_format_rejects_unsupported() {
        assert!(matches!(
            detect_format(&PathBuf::from("foo.doc")),
            Err(PersonalError::UnsupportedExtension(_))
        ));
        assert!(matches!(
            detect_format(&PathBuf::from("noext")),
            Err(PersonalError::UnsupportedExtension(_))
        ));
    }
}
