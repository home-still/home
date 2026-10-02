use crate::error::{PersonalError, Result};
use hs_scribe::epub::EpubLimits;

/// EPUB → markdown via the one bounded reader in `hs_scribe::epub` (the same
/// chapter walker as the academic pipeline, so personal EPUBs produce
/// structurally compatible markdown), under the configured `scribe.epub.*`
/// limits: how large an EPUB may expand to is one setting for the watcher,
/// the inbox and personal ingest alike.
pub fn convert(bytes: Vec<u8>) -> Result<String> {
    let limits = hs_scribe::config::ScribeConfig::load()
        .map_err(|e| PersonalError::Converter {
            format: "epub",
            source: anyhow::anyhow!("cannot read the scribe.epub limits: {e}"),
        })?
        .epub;
    convert_with(&limits, &bytes)
}

/// [`convert`] under explicit limits.
pub fn convert_with(limits: &EpubLimits, bytes: &[u8]) -> Result<String> {
    hs_scribe::epub::convert_epub_to_markdown_with(bytes, limits).map_err(|e| {
        PersonalError::Converter {
            format: "epub",
            source: e,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An EPUB whose spine lists one ~600 KB chapter `refs` times.
    fn epub_listing_one_chapter(refs: usize) -> Vec<u8> {
        let mut chapter = String::from("<html><body><h1>Only chapter</h1>");
        while chapter.len() < 600_000 {
            chapter.push_str("<p>lorem ipsum dolor sit amet consectetur</p>");
        }
        chapter.push_str("</body></html>");
        let mut opf = String::from(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="c" href="c.xhtml" media-type="application/xhtml+xml"/></manifest><spine>"#,
        );
        for _ in 0..refs {
            opf.push_str(r#"<itemref idref="c"/>"#);
        }
        opf.push_str("</spine></package>");
        let container = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in [
            ("META-INF/container.xml", container.as_bytes()),
            ("content.opf", opf.as_bytes()),
            ("c.xhtml", chapter.as_bytes()),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn personal_epubs_go_through_the_bounded_reader() {
        // A spine repeating one chapter 300 times used to be read 300 times
        // (180 MB of markdown from a ~6 KB archive).
        let book = epub_listing_one_chapter(300);
        let md = convert_with(&EpubLimits::default(), &book).unwrap();
        assert!(md.contains("# Only chapter"));
        assert!(md.len() < 1_000_000, "{} bytes", md.len());
    }

    #[test]
    fn the_limits_given_are_the_limits_applied() {
        let book = epub_listing_one_chapter(1);
        let tight = EpubLimits {
            max_entries: 100,
            max_entry_bytes: 100_000,
            max_total_bytes: 200_000,
        };
        let err = convert_with(&tight, &book).unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
    }
}
