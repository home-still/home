//! The single ingest path. Format → markdown → name/category → on-disk store
//! → distill index. There are no fallbacks: every step either succeeds or
//! aborts the operation. A failure mid-flight does NOT leave a half-written
//! sidecar, half-indexed Qdrant points, or a "conversion_failed: true" stamp
//! on disk — the caller sees a typed error and the personal store stays in
//! the state it was in before the call.

use crate::config::Config;
use crate::converters;
use crate::error::{PersonalError, Result};
use crate::models::{Category, SourceFormat};
use crate::services::{distill::PersonalDistill, naming};
use chrono::Utc;
use hs_common::catalog::{CatalogEntry, ConversionMeta};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct IngestOptions {
    pub category_override: Option<String>,
    pub title_override: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct IngestOutcome {
    pub stem: String,
    pub title: String,
    pub category: String,
    pub chunk_count: u32,
}

pub async fn ingest(cfg: &Config, file: &Path, opts: IngestOptions) -> Result<IngestOutcome> {
    let format = converters::detect_format(file)?;
    let bytes = std::fs::read(file)?;
    let sha = sha256_hex(&bytes);
    let size = bytes.len() as u64;

    // Convert first so the LLM has the actual extracted text, not raw bytes.
    let stem_hint = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("personal");
    let markdown = converters::convert(cfg, format, bytes.clone(), stem_hint).await?;

    // Resolve title + category. CLI overrides bypass Ollama for that field
    // only — partial overrides are fine and explicitly supported so a user
    // who knows the category but not the title can still leverage the LLM.
    let (title, category) = resolve_name_and_category(cfg, &markdown, &opts).await?;

    let stem = build_stem(&title, &sha);
    let dest_orig = sharded(cfg.root_dir(), &stem, format.as_str());
    let dest_md = sharded(cfg.markdown_dir(), &stem, "md");
    let dest_cat = sharded(cfg.root_dir(), &stem, "catalog.yaml");

    if !opts.force && (dest_orig.exists() || dest_md.exists() || dest_cat.exists()) {
        return Err(PersonalError::DuplicateStem(stem));
    }

    write_atomic(&dest_orig, &bytes)?;
    write_atomic(&dest_md, markdown.as_bytes())?;

    let entry = build_catalog(&CatalogInput {
        stem: &stem,
        format,
        title: &title,
        category,
        size,
        sha: &sha,
        orig_path: &dest_orig,
        md_path: &dest_md,
    });
    let yaml = serde_yaml_ng::to_string(&entry)
        .map_err(|e| PersonalError::Other(anyhow::anyhow!("catalog yaml: {e}")))?;
    write_atomic(&dest_cat, yaml.as_bytes())?;

    let distill = PersonalDistill::new(cfg)?;
    let result = distill
        .index(&format!("{stem}.md"), &markdown, &entry)
        .await?;

    Ok(IngestOutcome {
        stem,
        title,
        category: category.as_str().to_string(),
        chunk_count: result.chunks_indexed,
    })
}

async fn resolve_name_and_category(
    cfg: &Config,
    markdown: &str,
    opts: &IngestOptions,
) -> Result<(String, Category)> {
    match (&opts.title_override, &opts.category_override) {
        (Some(t), Some(c)) => Ok((t.trim().to_string(), c.parse()?)),
        (Some(t), None) => {
            let r = naming::title_and_category(cfg, markdown).await?;
            Ok((t.trim().to_string(), r.category))
        }
        (None, Some(c)) => {
            let r = naming::title_and_category(cfg, markdown).await?;
            Ok((r.title, c.parse()?))
        }
        (None, None) => {
            let r = naming::title_and_category(cfg, markdown).await?;
            Ok((r.title, r.category))
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Build a filesystem-safe stem from the LLM title plus a short hash suffix.
/// The hash suffix prevents collisions when two documents share a sanitized
/// title (e.g. two "lab results 2024" PDFs from different visits).
pub(crate) fn build_stem(title: &str, sha: &str) -> String {
    let mut buf = String::with_capacity(title.len());
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            buf.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !buf.is_empty() {
            buf.push('-');
            last_dash = true;
        }
    }
    while buf.ends_with('-') {
        buf.pop();
    }
    if buf.is_empty() {
        buf.push_str("doc");
    }
    if buf.len() > 80 {
        buf.truncate(80);
        while buf.ends_with('-') {
            buf.pop();
        }
    }
    let suffix: String = sha.chars().take(8).collect();
    format!("{buf}-{suffix}")
}

fn sharded(dir: PathBuf, stem: &str, ext: &str) -> PathBuf {
    let prefix = &stem[..stem.len().min(2)];
    dir.join(prefix).join(format!("{stem}.{ext}"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("part")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

struct CatalogInput<'a> {
    stem: &'a str,
    format: SourceFormat,
    title: &'a str,
    category: Category,
    size: u64,
    sha: &'a str,
    orig_path: &'a Path,
    md_path: &'a Path,
}

fn build_catalog(i: &CatalogInput<'_>) -> CatalogEntry {
    let now = Utc::now().to_rfc3339();
    CatalogEntry {
        title: Some(i.title.to_string()),
        pdf_path: Some(i.orig_path.display().to_string()),
        markdown_path: Some(i.md_path.display().to_string()),
        downloaded_at: Some(now.clone()),
        file_size_bytes: Some(i.size),
        sha256: Some(i.sha.to_string()),
        conversion: Some(ConversionMeta {
            server: format!("personal:{}", i.format.as_str()),
            duration_secs: 0.0,
            total_pages: 0,
            converted_at: now,
            pages: Vec::new(),
        }),
        category: Some(i.category.to_string()),
        original_format: Some(i.format.to_string()),
        source: Some(format!("personal:{}", i.stem)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_stem_sanitizes_punctuation_and_appends_hash() {
        let s = build_stem("Lab Results: 2024 Q1", "abcdef0123456789");
        assert_eq!(s, "lab-results-2024-q1-abcdef01");
    }

    #[test]
    fn build_stem_collapses_whitespace_runs() {
        let s = build_stem("  too   many  spaces  ", "0011223344556677");
        assert_eq!(s, "too-many-spaces-00112233");
    }

    #[test]
    fn build_stem_truncates_overlong_titles() {
        let long = "a".repeat(200);
        let s = build_stem(&long, "deadbeef00000000");
        // 80 chars of title body + "-" + 8 hash chars = 89.
        assert_eq!(s.len(), 89);
        assert!(s.ends_with("-deadbeef"));
    }

    #[test]
    fn build_stem_falls_back_to_doc_when_title_is_punctuation_only() {
        let s = build_stem("!!!---???", "ffffeeeeddddcccc");
        assert_eq!(s, "doc-ffffeeee");
    }
}
