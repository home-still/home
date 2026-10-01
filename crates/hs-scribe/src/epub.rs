//! EPUB → markdown converter. First-class ingest path alongside PDF (scribe
//! VLM) and HTML (scraper). One path: parse the EPUB archive, walk the spine
//! in reading order, convert each chapter's XHTML through
//! [`crate::html::convert_html_to_markdown`], concatenate. Errors propagate;
//! there is no silent stub or empty-output gate — if the converter can't
//! produce markdown, the operation fails and the caller logs it.
//!
//! An EPUB is a zip, and the zip bytes are untrusted. Before any chapter is
//! decompressed the whole archive is walked once under [`EpubLimits`]: an
//! entry-count cap, a per-entry cap and a total-expanded-bytes cap, each
//! enforced on the bytes the decompressor actually produces (the sizes a
//! zip header declares are exactly what a zip bomb lies about).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};

/// Caps on what an EPUB archive may expand to. Exceeding any of them is an
/// error (the document is refused), never a truncated conversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpubLimits {
    /// Most entries (files and directories) the archive may hold. Real
    /// books have hundreds to a few thousand.
    pub max_entries: usize,
    /// Largest any one entry may expand to, in bytes.
    pub max_entry_bytes: u64,
    /// Largest the whole archive may expand to, in bytes.
    pub max_total_bytes: u64,
}

impl Default for EpubLimits {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_entry_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}

impl EpubLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_entries == 0 || self.max_entry_bytes == 0 || self.max_total_bytes == 0 {
            bail!("epub limits must all be at least 1: {self:?}");
        }
        if self.max_entry_bytes > self.max_total_bytes {
            bail!(
                "epub.max_entry_bytes ({}) exceeds epub.max_total_bytes ({})",
                self.max_entry_bytes,
                self.max_total_bytes
            );
        }
        Ok(())
    }
}

/// Walk every entry of the archive, decompressing into a sink, and fail as
/// soon as a cap is crossed. Nothing is retained.
pub fn check_archive(bytes: &[u8], limits: &EpubLimits) -> Result<()> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(bytes)).context("EPUB is not a readable zip archive")?;
    if zip.len() > limits.max_entries {
        bail!(
            "EPUB has {} entries, over the limit of {}",
            zip.len(),
            limits.max_entries
        );
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .with_context(|| format!("EPUB entry {i} is unreadable"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if entry.size() > limits.max_entry_bytes {
            bail!(
                "EPUB entry `{name}` declares {} bytes, over the per-entry limit of {}",
                entry.size(),
                limits.max_entry_bytes
            );
        }
        // One byte past the allowance proves the entry is too big without
        // producing the rest of it.
        let allowance = limits.max_entry_bytes.min(limits.max_total_bytes - total);
        let produced = std::io::copy(&mut entry.take(allowance + 1), &mut std::io::sink())
            .with_context(|| format!("EPUB entry `{name}` failed to decompress"))?;
        if produced > limits.max_entry_bytes {
            bail!(
                "EPUB entry `{name}` expands past the per-entry limit of {} bytes",
                limits.max_entry_bytes
            );
        }
        total += produced;
        if total > limits.max_total_bytes {
            bail!(
                "EPUB expands past the total limit of {} bytes",
                limits.max_total_bytes
            );
        }
    }
    Ok(())
}

/// Convert an EPUB archive's bytes to markdown under the default
/// [`EpubLimits`]. For callers with no configuration to consult.
pub fn convert_epub_to_markdown(bytes: &[u8]) -> Result<String> {
    convert_epub_to_markdown_with(bytes, &EpubLimits::default())
}

/// Convert an EPUB archive's bytes to markdown. Iterates each document in
/// spine order; each chapter's XHTML is fed through the shared HTML walker
/// so the two paths produce structurally compatible markdown.
pub fn convert_epub_to_markdown_with(bytes: &[u8], limits: &EpubLimits) -> Result<String> {
    check_archive(bytes, limits)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    fn small() -> EpubLimits {
        EpubLimits {
            max_entries: 8,
            max_entry_bytes: 1 << 20,
            max_total_bytes: 3 << 20,
        }
    }

    #[test]
    fn a_highly_compressible_entry_over_the_cap_is_refused_without_inflating_it() {
        // 64 MiB of zeros deflates to ~64 KiB: the archive itself is tiny.
        let bomb = zip_of(&[("bomb.xhtml", vec![0u8; 64 << 20])]);
        assert!(
            bomb.len() < 200_000,
            "fixture must be small: {}",
            bomb.len()
        );
        let err = check_archive(&bomb, &small()).unwrap_err();
        assert!(err.to_string().contains("per-entry limit"), "{err:#}");
    }

    #[test]
    fn many_entries_that_each_fit_but_not_together_are_refused() {
        // Each entry is within the per-entry cap; their sum is not.
        let chunk = vec![b'a'; 900 * 1024];
        let entries: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("c{i}.xhtml"), chunk.clone()))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let err = check_archive(&zip_of(&refs), &small()).unwrap_err();
        assert!(err.to_string().contains("total limit"), "{err:#}");
    }

    #[test]
    fn too_many_entries_are_refused_before_any_is_read() {
        let entries: Vec<(String, Vec<u8>)> =
            (0..9).map(|i| (format!("e{i}.txt"), vec![b'x'])).collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let err = check_archive(&zip_of(&refs), &small()).unwrap_err();
        assert!(err.to_string().contains("9 entries"), "{err:#}");
    }

    #[test]
    fn an_archive_within_every_cap_passes() {
        let z = zip_of(&[("a.xhtml", vec![b'a'; 1000]), ("b.xhtml", vec![b'b'; 2000])]);
        check_archive(&z, &small()).unwrap();
        // Exactly at the per-entry cap is allowed.
        let at_cap = zip_of(&[("a.bin", vec![1u8; 1 << 20])]);
        check_archive(&at_cap, &small()).unwrap();
    }

    #[test]
    fn a_non_zip_is_an_error_not_a_panic() {
        assert!(check_archive(b"not a zip at all", &small()).is_err());
        assert!(convert_epub_to_markdown(b"").is_err());
    }

    #[test]
    fn conversion_refuses_a_bomb_before_decompressing_a_chapter() {
        let bomb = zip_of(&[("OEBPS/c1.xhtml", vec![b' '; 70 << 20])]);
        let err = convert_epub_to_markdown_with(&bomb, &small()).unwrap_err();
        assert!(err.to_string().contains("limit"), "{err:#}");
    }

    #[test]
    fn limits_must_be_positive_and_consistent() {
        EpubLimits::default().validate().unwrap();
        for bad in [
            EpubLimits {
                max_entries: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_entry_bytes: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_total_bytes: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_entry_bytes: 10,
                max_total_bytes: 5,
                ..EpubLimits::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
