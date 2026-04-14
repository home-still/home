use hs_common::catalog::CatalogEntry;
use regex::Regex;

use crate::types::DocumentMeta;

/// Extract metadata from catalog YAML + regex patterns in the markdown text.
pub fn extract_rule_based(
    markdown: &str,
    stem: &str,
    markdown_path: &str,
    catalog: Option<&CatalogEntry>,
) -> DocumentMeta {
    let mut meta = DocumentMeta {
        doc_id: stem.to_string(),
        markdown_path: markdown_path.to_string(),
        ..Default::default()
    };

    // Pull from catalog if available
    if let Some(cat) = catalog {
        meta.title = cat.title.clone();
        meta.authors = cat.authors.iter().map(|a| a.name.clone()).collect();
        meta.doi = cat.doi.clone();
        meta.publication_date = cat.publication_date.clone();
        meta.abstract_text = cat.abstract_text.clone();
        meta.cited_by_count = cat.cited_by_count;
        meta.source = cat.source.clone();
        meta.pdf_path = cat.pdf_path.clone();
    }

    // Regex fill-in from markdown text
    let first_lines: String = markdown.lines().take(50).collect::<Vec<_>>().join("\n");

    // DOI
    if meta.doi.is_none() {
        if let Some(doi) = extract_doi(markdown) {
            meta.doi = Some(doi);
        }
    }

    // Year
    if meta.publication_date.is_none() {
        if let Some(year) = extract_year(&first_lines) {
            meta.publication_date = Some(year);
        }
    }

    meta
}

fn extract_doi(text: &str) -> Option<String> {
    let re = Regex::new(r"10\.\d{4,}/[^\s,\]>)]+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

fn extract_year(text: &str) -> Option<String> {
    let re = Regex::new(r"\b(19|20)\d{2}\b").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

/// LLM-powered keyword/topic extraction via Ollama (optional).
#[cfg(feature = "server")]
pub async fn extract_llm_metadata(
    text_sample: &str,
    ollama_url: &str,
    model: &str,
) -> Result<(Vec<String>, Vec<String>), crate::error::DistillError> {
    use ollama_rs::generation::completion::request::GenerationRequest;
    use ollama_rs::Ollama;

    // Parse host and port from URL string
    let trimmed = ollama_url
        .strip_prefix("http://")
        .or_else(|| ollama_url.strip_prefix("https://"))
        .unwrap_or(ollama_url);
    let (host_part, port) = match trimmed.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(11434)),
        None => (trimmed, 11434),
    };
    let scheme = if ollama_url.starts_with("https") {
        "https"
    } else {
        "http"
    };

    let ollama = Ollama::new(format!("{scheme}://{host_part}"), port);

    let prompt = format!(
        "Extract 5-10 keywords and 2-3 academic topics from this text. \
         Return ONLY valid JSON: {{\"keywords\": [...], \"topics\": [...]}}\n\n\
         Text:\n{}\n\nJSON:",
        &text_sample[..text_sample.len().min(2000)]
    );

    let request = GenerationRequest::new(model.to_string(), prompt);

    let response = ollama
        .generate(request)
        .await
        .map_err(|e| crate::error::DistillError::Metadata(format!("Ollama error: {e}")))?;

    // Try to parse JSON from response
    let text = response.response.trim();
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(val) => {
            let keywords = val["keywords"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let topics = val["topics"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Ok((keywords, topics))
        }
        Err(_) => {
            tracing::warn!("Failed to parse LLM metadata response as JSON");
            Ok((Vec::new(), Vec::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_doi_from_text() {
        let text = "See https://doi.org/10.1234/abcd.5678 for details.";
        let doi = extract_doi(text);
        assert_eq!(doi, Some("10.1234/abcd.5678".to_string()));
    }

    #[test]
    fn extract_year_from_text() {
        let text = "Published in 2023 by the authors.";
        let year = extract_year(text);
        assert_eq!(year, Some("2023".to_string()));
    }

    #[test]
    fn no_doi_returns_none() {
        let text = "No DOI here.";
        assert_eq!(extract_doi(text), None);
    }
}
