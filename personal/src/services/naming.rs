//! Title + category extraction via Ollama. The contract is strict: we ask the
//! model for JSON of the form `{"title": "...", "category": "..."}`. If the
//! response is unreachable, malformed, or names a category outside the
//! configured taxonomy, we fail loudly. There is no filename fallback — that
//! defeats the point of having an LLM in the loop and quietly poisons the
//! catalog with garbage stems.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use crate::models::Category;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct NameResult {
    pub title: String,
    pub category: Category,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
}

#[derive(Debug, Deserialize)]
struct ModelOutput {
    title: String,
    category: String,
}

const PROMPT_PREFIX: &str = "You are tagging a personal document for a private archive. \
Read the excerpt below and reply with a single JSON object on one line, no prose, \
no markdown fences, with exactly two string fields: \
\"title\" (5-10 words, no quotes, no trailing punctuation) and \
\"category\" (one of: medical, financial, education, legal, employment, tax, insurance, correspondence, other). \
Pick the closest category; if nothing fits, use \"other\".\n\nDocument excerpt:\n";

pub async fn title_and_category(cfg: &Config, markdown: &str) -> Result<NameResult> {
    let excerpt = take_chars(markdown, cfg.naming.max_input_tokens.saturating_mul(4));
    let prompt = format!("{PROMPT_PREFIX}{excerpt}\n\nJSON:");

    let body = serde_json::json!({
        "model": cfg.naming.model,
        "prompt": prompt,
        "stream": false,
        "format": "json",
    });

    let url = format!(
        "{}/api/generate",
        cfg.naming.ollama_url.trim_end_matches('/')
    );
    // Canonical workspace client constructor (rc.306 deleted the ad-hoc
    // builders): sets connect_timeout alongside the overall timeout, so a
    // host that accepts the SYN but never completes the handshake fails
    // fast instead of hanging the full 120s.
    let http = hs_common::http::http_client(std::time::Duration::from_secs(120))
        .map_err(|e| PersonalError::Naming(format!("http client: {e}")))?;

    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| PersonalError::Naming(format!("ollama unreachable at {url}: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PersonalError::Naming(format!(
            "ollama returned {status}: {body}"
        )));
    }

    let outer: OllamaResponse = resp
        .json()
        .await
        .map_err(|e| PersonalError::Naming(format!("ollama envelope parse: {e}")))?;

    let parsed: ModelOutput = serde_json::from_str(outer.response.trim()).map_err(|e| {
        PersonalError::Naming(format!(
            "ollama response was not the expected JSON shape: {e}; raw: {}",
            outer.response
        ))
    })?;

    let title = parsed.title.trim().to_string();
    if title.is_empty() {
        return Err(PersonalError::Naming("model returned empty title".into()));
    }
    if title.len() > 200 {
        return Err(PersonalError::Naming(format!(
            "model returned over-long title ({} chars)",
            title.len()
        )));
    }

    let category: Category = parsed.category.trim().parse()?;

    Ok(NameResult { title, category })
}

fn take_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, NamingConfig};

    fn cfg_with_url(url: &str) -> Config {
        Config {
            naming: NamingConfig {
                ollama_url: url.to_string(),
                model: "test-model".to_string(),
                max_input_tokens: 64,
            },
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn parses_well_formed_response() {
        let mut server = mockito::Server::new_async().await;
        let envelope = serde_json::json!({
            "response": "{\"title\":\"Lab Results 2024\",\"category\":\"medical\"}"
        });
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let r = title_and_category(&cfg, "anything")
            .await
            .expect("naming should succeed");
        assert_eq!(r.title, "Lab Results 2024");
        assert_eq!(r.category, crate::models::Category::Medical);
    }

    #[tokio::test]
    async fn rejects_category_outside_taxonomy() {
        let mut server = mockito::Server::new_async().await;
        let envelope = serde_json::json!({
            "response": "{\"title\":\"Mystery Doc\",\"category\":\"sports\"}"
        });
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        assert!(
            matches!(err, crate::error::PersonalError::UnknownCategory(_)),
            "expected UnknownCategory, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_inner_json() {
        let mut server = mockito::Server::new_async().await;
        let envelope = serde_json::json!({
            "response": "this is not json at all"
        });
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not the expected JSON shape"),
            "expected JSON shape error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_empty_title() {
        let mut server = mockito::Server::new_async().await;
        let envelope = serde_json::json!({
            "response": "{\"title\":\"   \",\"category\":\"medical\"}"
        });
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty title"), "got: {msg}");
    }

    #[tokio::test]
    async fn fails_loudly_on_5xx() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(503)
            .with_body("model loading")
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("503"), "expected status in error: {msg}");
    }
}
