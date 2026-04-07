use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CatalogEntry {
    // Paper metadata (from search/download)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<AuthorEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cited_by_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub download_urls: Vec<String>,

    // File references
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown_path: Option<String>,

    // Download metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

    // Conversion metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversion: Option<ConversionMeta>,

    // Embedding metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionMeta {
    pub server: String,
    pub duration_secs: u64,
    pub total_pages: u64,
    pub converted_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PageOffset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingMeta {
    pub server: String,
    pub chunks_indexed: u32,
    pub compute_device: String,
    pub embedded_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageOffset {
    pub page: usize,
    pub char_start: usize,
    pub char_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorEntry {
    pub name: String,
}

/// Compute page-to-character offsets from the final markdown.
/// Pages are separated by `\n\n---\n\n` in the joined output.
pub fn compute_page_offsets(markdown: &str) -> Vec<PageOffset> {
    let separator = "\n\n---\n\n";
    let mut offsets = Vec::new();
    let mut pos = 0;
    for (i, page) in markdown.split(separator).enumerate() {
        offsets.push(PageOffset {
            page: i + 1,
            char_start: pos,
            char_end: pos + page.len(),
        });
        pos += page.len() + separator.len();
    }
    offsets
}

/// Read an existing catalog entry, or return None if it doesn't exist.
pub fn read_catalog_entry(catalog_dir: &Path, stem: &str) -> Option<CatalogEntry> {
    let path = catalog_dir.join(format!("{stem}.yaml"));
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_yaml_ng::from_str(&contents).ok()
}

/// Write a catalog entry to disk (atomic write).
pub fn write_catalog_entry(catalog_dir: &Path, stem: &str, entry: &CatalogEntry) {
    let _ = std::fs::create_dir_all(catalog_dir);
    let path = catalog_dir.join(format!("{stem}.yaml"));
    if let Ok(yaml) = serde_yaml_ng::to_string(entry) {
        // Use atomic write if tempfile is available
        let _ = std::fs::write(&path, yaml);
    }
}

/// Update only the conversion section of an existing catalog entry.
/// If no entry exists, creates a minimal one with just conversion metadata.
pub fn update_conversion_catalog(
    catalog_dir: &Path,
    stem: &str,
    server: &str,
    duration_secs: u64,
    total_pages: u64,
    pages: Vec<PageOffset>,
    markdown_path: &str,
) {
    let mut entry = read_catalog_entry(catalog_dir, stem).unwrap_or_default();

    entry.markdown_path = Some(markdown_path.to_string());
    entry.conversion = Some(ConversionMeta {
        server: server.to_string(),
        duration_secs,
        total_pages,
        converted_at: chrono::Utc::now().to_rfc3339(),
        pages,
    });

    write_catalog_entry(catalog_dir, stem, &entry);
}

/// Update only the embedding section of an existing catalog entry.
/// If no entry exists, creates a minimal one with just embedding metadata.
pub fn update_embedding_catalog(
    catalog_dir: &Path,
    stem: &str,
    server: &str,
    chunks_indexed: u32,
    compute_device: &str,
) {
    let mut entry = read_catalog_entry(catalog_dir, stem).unwrap_or_default();

    entry.embedding = Some(EmbeddingMeta {
        server: server.to_string(),
        chunks_indexed,
        compute_device: compute_device.to_string(),
        embedded_at: chrono::Utc::now().to_rfc3339(),
    });

    write_catalog_entry(catalog_dir, stem, &entry);
}
