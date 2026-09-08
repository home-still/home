use hs_common::catalog::CatalogEntry;

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
        meta.category = cat.category.clone();
        meta.original_format = cat.original_format.clone();
        meta.ingested_at = cat.downloaded_at.clone();
    }

    // Always derive pdf_path from the sharded storage key rather than the
    // catalog's `pdf_path` field — that field is a host-local filesystem
    // path (e.g. `/Users/.../papers/10/DOI.pdf`) written by whichever
    // machine ran the downloader, and leaking it through search results
    // exposes the downloader's home directory. The storage key is the
    // canonical location and is identical across hosts.
    meta.pdf_path = Some(format!("papers/{}", hs_common::sharded_key(stem, "pdf")));

    // Neither DOI nor year is ever regex-extracted from body text. The
    // first DOI-shaped string in a paper is almost always a reference
    // citation, not the paper's own DOI, which produced mislabeled chunks
    // in rc.<=230. The first 4-digit year in the opening lines has the
    // same problem and was worse, because it silently succeeded: journal
    // headers, copyright lines and the first citation all match, so a 2021
    // Frontiers paper was indexed as 2008. A wrong year is more damaging
    // than a missing one — it survives into search results and citations
    // with no signal that it was guessed. When the catalog has no
    // publication_date, the year stays None; backfill the catalog instead.

    meta
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
    fn year_is_never_guessed_from_body_text() {
        // Opening lines carry a copyright year and a citation year that are
        // not the paper's own. With no catalog date, the year must stay
        // None rather than pick one of them.
        let markdown = "Frontiers in Psychology\n\nCopyright 2008 the authors\n\n\
                        See Smith et al. 1998 for background.\n\nAbstract";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.publication_date, None);
    }

    #[test]
    fn year_comes_from_the_catalog_when_present() {
        let cat = hs_common::catalog::CatalogEntry {
            publication_date: Some("2021-03-04".into()),
            ..Default::default()
        };
        let meta = extract_rule_based("Copyright 2008", "stem", "path/stem.md", Some(&cat));
        assert_eq!(meta.publication_date.as_deref(), Some("2021-03-04"));
    }

    #[test]
    fn doi_comes_only_from_catalog_not_body_text() {
        // Body text mentions another paper's DOI in a citation — must be ignored.
        let markdown = "... see 10.1002/aur.2049 for background ...";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.doi, None);
    }
}
