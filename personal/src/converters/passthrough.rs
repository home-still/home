use crate::error::{PersonalError, Result};

/// Markdown is a verbatim passthrough. We still validate UTF-8 — bad encoding
/// at ingest is a hard error, not a "best-effort" lossy decode.
pub fn convert_markdown(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|e| PersonalError::Converter {
        format: "markdown",
        source: anyhow::anyhow!("not valid UTF-8: {e}"),
    })
}

/// Plain text passes through with no transformation. Most personal txt files
/// are short notes or exports where preserving the literal content matters
/// more than coercing it into structured markdown.
pub fn convert_txt(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|e| PersonalError::Converter {
        format: "txt",
        source: anyhow::anyhow!("not valid UTF-8: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_passthrough_preserves_bytes() {
        let md = "# Heading\n\nSome **bold** text.\n";
        let out = convert_markdown(md.as_bytes().to_vec()).unwrap();
        assert_eq!(out, md);
    }

    #[test]
    fn txt_passthrough_preserves_bytes() {
        let txt = "line one\nline two\n";
        let out = convert_txt(txt.as_bytes().to_vec()).unwrap();
        assert_eq!(out, txt);
    }

    #[test]
    fn invalid_utf8_is_a_hard_error() {
        let bad = vec![0xFF, 0xFE, 0xFD];
        assert!(convert_markdown(bad.clone()).is_err());
        assert!(convert_txt(bad).is_err());
    }
}
