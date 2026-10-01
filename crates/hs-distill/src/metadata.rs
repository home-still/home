use std::time::Duration;

use hs_common::catalog::CatalogEntry;
use serde::Deserialize;

use crate::error::DistillError;
use crate::types::DocumentMeta;

/// Extract metadata from the catalog entry. The body text (`_markdown`) is
/// deliberately never mined for DOI or year — see the note below.
pub fn extract_rule_based(
    _markdown: &str,
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

/// Bytes of document text shown to the model. Cut on a char boundary.
const LLM_SAMPLE_BYTES: usize = 2000;

/// Keywords and topics proposed by the metadata model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMetadata {
    pub keywords: Vec<String>,
    pub topics: Vec<String>,
}

/// Parse and check the configured Ollama base URL: http(s) with a host. An
/// invalid URL (including an unparseable port) is an error, never a default.
pub fn parse_ollama_url(ollama_url: &str) -> Result<reqwest::Url, DistillError> {
    let url = reqwest::Url::parse(ollama_url)
        .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?} is invalid: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(DistillError::Config(format!(
            "ollama_url {ollama_url:?} must be an http(s) URL with a host"
        )));
    }
    Ok(url)
}

fn llm_prompt(text_sample: &str) -> String {
    format!(
        "Extract 5-10 keywords and 2-3 academic topics from this text. \
         Return ONLY valid JSON: {{\"keywords\": [...], \"topics\": [...]}}\n\n\
         Text:\n{}\n\nJSON:",
        crate::text::prefix_at_most(text_sample, LLM_SAMPLE_BYTES)
    )
}

#[derive(Deserialize)]
struct GenerateReply {
    response: String,
}

#[derive(Deserialize)]
struct ExtractedMetadata {
    keywords: Vec<String>,
    topics: Vec<String>,
}

/// Parse the model's `response` text. It must be exactly the requested JSON
/// object; anything else is an error so a bad reply can never be mistaken
/// for "the model found no keywords".
fn parse_llm_reply(response: &str) -> Result<LlmMetadata, DistillError> {
    let parsed: ExtractedMetadata = serde_json::from_str(response.trim()).map_err(|e| {
        DistillError::Metadata(format!(
            "LLM reply is not the requested {{\"keywords\":[..],\"topics\":[..]}} object: {e}"
        ))
    })?;
    Ok(LlmMetadata {
        keywords: parsed.keywords,
        topics: parsed.topics,
    })
}

/// LLM-powered keyword/topic extraction via Ollama's `/api/generate`
/// (optional, `llm_metadata: true`). Every failure is an `Err`: unreachable
/// or slow server (`timeout` bounds the whole call), non-2xx status, or a
/// reply that is not the requested JSON.
pub async fn extract_llm_metadata(
    text_sample: &str,
    ollama_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<LlmMetadata, DistillError> {
    let endpoint = parse_ollama_url(ollama_url)?
        .join("api/generate")
        .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?}: {e}")))?;

    let http = hs_common::http::client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .build()
        .map_err(|e| DistillError::Metadata(format!("failed to build Ollama client: {e}")))?;

    let resp = http
        .post(endpoint)
        .json(&serde_json::json!({
            "model": model,
            "prompt": llm_prompt(text_sample),
            "stream": false,
            "format": "json",
        }))
        .send()
        .await
        .map_err(|e| DistillError::Metadata(format!("Ollama request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(DistillError::Metadata(format!(
            "Ollama returned {status}: {}",
            crate::text::prefix_at_most(&body, 300)
        )));
    }
    let reply: GenerateReply = resp
        .json()
        .await
        .map_err(|e| DistillError::Metadata(format!("Ollama reply is not valid JSON: {e}")))?;
    parse_llm_reply(&reply.response)
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

    // ── LLM metadata (RA-13) ───────────────────────────────────────────

    fn lines_of(prompt: &str) -> &str {
        prompt
            .split("Text:\n")
            .nth(1)
            .and_then(|t| t.split("\n\nJSON:").next())
            .unwrap()
    }

    #[test]
    fn prompt_sample_never_splits_a_multibyte_char() {
        // "€" is 3 bytes, so byte 2000 lands inside a char for 2 of the 3
        // alignments. The old `&text[..text.len().min(2000)]` panicked.
        for lead in 0..3 {
            let text = format!("{}{}", "x".repeat(lead), "€".repeat(1000));
            let prompt = llm_prompt(&text);
            let sample = lines_of(&prompt);
            assert!(sample.len() <= LLM_SAMPLE_BYTES, "lead {lead}");
            assert!(
                sample.len() > LLM_SAMPLE_BYTES - 3,
                "lead {lead}: cut too early ({} bytes)",
                sample.len()
            );
            assert!(text.starts_with(sample));
        }
    }

    #[test]
    fn prompt_sample_keeps_short_text_whole() {
        let prompt = llm_prompt("café ☕");
        assert_eq!(lines_of(&prompt), "café ☕");
    }

    #[test]
    fn ollama_url_must_be_a_valid_http_url() {
        for bad in [
            "http://localhost:notaport",
            "http://localhost:99999",
            "localhost:11434",
            "ftp://localhost:11434",
            "",
        ] {
            assert!(
                matches!(parse_ollama_url(bad), Err(DistillError::Config(_))),
                "{bad:?} must be rejected"
            );
        }
        let ok = parse_ollama_url("http://127.0.0.1:11434").unwrap();
        assert_eq!(ok.port(), Some(11434));
    }

    #[test]
    fn reply_must_be_the_requested_object() {
        let ok = parse_llm_reply(r#" {"keywords":["a","b"],"topics":["t"]} "#).unwrap();
        assert_eq!(ok.keywords, ["a", "b"]);
        assert_eq!(ok.topics, ["t"]);

        for bad in [
            "I could not find any keywords",
            r#"{"keywords":["a"]}"#,
            r#"{"keywords":"a","topics":["t"]}"#,
            "```json\n{\"keywords\":[],\"topics\":[]}\n```",
            "",
        ] {
            assert!(
                matches!(parse_llm_reply(bad), Err(DistillError::Metadata(_))),
                "{bad:?} must not parse into metadata"
            );
        }
    }

    fn generate_reply(inner: &str) -> String {
        serde_json::json!({ "response": inner }).to_string()
    }

    #[tokio::test]
    async fn llm_extraction_posts_to_generate_and_returns_the_parsed_reply() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| {
            Reply::Json(
                200,
                generate_reply(r#"{"keywords":["k1","k2"],"topics":["t1"]}"#),
            )
        })
        .await;
        let got = extract_llm_metadata("héllo wörld", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got.keywords, ["k1", "k2"]);
        assert_eq!(got.topics, ["t1"]);

        let seen = fake.recorded();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].request_line.starts_with("POST /api/generate"));
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], false);
        assert_eq!(body["format"], "json");
        assert!(body["prompt"].as_str().unwrap().contains("héllo wörld"));
    }

    #[tokio::test]
    async fn unparseable_model_reply_is_an_error_not_empty_metadata() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Json(200, generate_reply("sorry, I cannot do that"))).await;
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
    }

    #[tokio::test]
    async fn ollama_error_status_is_an_error() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Json(404, r#"{"error":"model not found"}"#.into())).await;
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
        assert!(err.to_string().contains("model not found"), "{err}");
    }

    #[tokio::test]
    async fn stalled_ollama_hits_the_timeout() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Hang).await;
        let started = std::time::Instant::now();
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout did not bound the call"
        );
    }
}
