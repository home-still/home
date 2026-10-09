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

const PROMPT_HEAD: &str = "You are tagging a personal document for a private archive. \
Read the excerpt below and reply with a single JSON object on one line, no prose, \
no markdown fences, with exactly two string fields: \
\"title\" (5-10 words, no quotes, no trailing punctuation) and ";

/// The prompt offers exactly `personal.categories`: offering a category the
/// config forbids would make the ingest fail on a pick the model was invited
/// to make.
fn build_prompt(categories: &[String], excerpt: &str) -> String {
    let list = categories.join(", ");
    let other = if categories.iter().any(|c| c.eq_ignore_ascii_case("other")) {
        " If nothing fits, use \"other\"."
    } else {
        ""
    };
    format!(
        "{PROMPT_HEAD}\"category\" (one of: {list}). Pick the closest category.{other}\
         \n\nDocument excerpt:\n{excerpt}\n\nJSON:"
    )
}

/// What the model said, with only its JSON shape checked. Each field is
/// validated by the accessor that needs it, so a caller that pinned the title
/// or the category (`--title`, `--category`) is not failed by a bad pick for
/// the field it overrode.
#[derive(Debug, Clone)]
pub struct Suggestion {
    title: String,
    category: String,
}

impl Suggestion {
    /// The model's title: non-empty and not absurdly long.
    pub fn title(&self) -> Result<String> {
        let title = self.title.trim().to_string();
        if title.is_empty() {
            return Err(PersonalError::Naming("model returned empty title".into()));
        }
        let title_chars = title.chars().count();
        if title_chars > 200 {
            return Err(PersonalError::Naming(format!(
                "model returned over-long title ({title_chars} chars)"
            )));
        }
        Ok(title)
    }

    /// The model's category, which must be in `personal.categories`.
    pub fn category(&self, cfg: &Config) -> Result<Category> {
        cfg.resolve_category(&self.category)
    }
}

/// Both fields, each validated.
pub async fn title_and_category(cfg: &Config, markdown: &str) -> Result<NameResult> {
    let suggestion = suggest(cfg, markdown).await?;
    Ok(NameResult {
        title: suggestion.title()?,
        category: suggestion.category(cfg)?,
    })
}

/// Ask the model to name `markdown`.
pub async fn suggest(cfg: &Config, markdown: &str) -> Result<Suggestion> {
    let excerpt = take_chars(markdown, cfg.naming.max_input_tokens.saturating_mul(4));
    let prompt = build_prompt(&cfg.categories, &excerpt);

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

    Ok(Suggestion {
        title: parsed.title,
        category: parsed.category,
    })
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

    #[test]
    fn take_chars_never_splits_a_multibyte_char() {
        // `é` is 2 bytes, `日` is 3: every `max` that lands inside one must
        // back off to the previous boundary instead of panicking in `[..end]`.
        let s = "aé日b";
        for max in 0..=s.len() + 2 {
            let out = take_chars(s, max);
            assert!(out.len() <= max, "max={max} out={out:?}");
            assert!(s.starts_with(&out), "max={max} out={out:?}");
        }
        assert_eq!(take_chars(s, 1), "a");
        assert_eq!(take_chars(s, 2), "a"); // byte 2 is inside `é`
        assert_eq!(take_chars(s, 3), "aé");
        assert_eq!(take_chars(s, 5), "aé"); // byte 5 is inside `日`
        assert_eq!(take_chars(s, 6), "aé日");
        assert_eq!(take_chars(s, 0), "");
        assert_eq!(take_chars("日本語", 1), "");
        assert_eq!(take_chars(s, s.len()), s);
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
    async fn offers_and_accepts_only_the_configured_categories() {
        let mut server = mockito::Server::new_async().await;
        let envelope = serde_json::json!({
            "response": "{\"title\":\"Annual Return Summary\",\"category\":\"tax\"}"
        });
        let _m = server
            .mock("POST", "/api/generate")
            .match_body(mockito::Matcher::Regex(
                r"one of: medical, other\)\. Pick the closest category\. If nothing fits".into(),
            ))
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = Config {
            categories: vec!["medical".into(), "other".into()],
            ..cfg_with_url(&server.url())
        };
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        assert!(
            matches!(&err, crate::error::PersonalError::UnknownCategory(c) if c == "tax"),
            "expected UnknownCategory(tax), got: {err:?}"
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
    async fn accepts_long_cjk_title_within_char_limit() {
        // 70 CJK chars ≈ 210 UTF-8 bytes. The length gate counts chars, so this
        // must pass; a byte-based check (the old bug) would reject it.
        let title: String = "研".repeat(70);
        let inner = serde_json::json!({ "title": title, "category": "medical" }).to_string();
        let envelope = serde_json::json!({ "response": inner });
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let r = title_and_category(&cfg, "anything")
            .await
            .expect("70-char CJK title should be accepted");
        assert_eq!(r.title.chars().count(), 70);
    }

    #[tokio::test]
    async fn rejects_over_long_title_by_char_count() {
        let title: String = "a".repeat(201);
        let inner = serde_json::json!({ "title": title, "category": "medical" }).to_string();
        let envelope = serde_json::json!({ "response": inner });
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(envelope.to_string())
            .create_async()
            .await;

        let cfg = cfg_with_url(&server.url());
        let err = title_and_category(&cfg, "anything").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("over-long title"), "got: {msg}");
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
