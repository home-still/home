use super::region::RegionType;
use anyhow::Result;

pub struct OpenAiBackend {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl OpenAiBackend {
    pub fn new(url: &str, model: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.trim_end_matches('/').to_string(),
            model: model.strip_suffix(":latest").unwrap_or(model).to_string(),
        }
    }

    pub async fn recognize(&self, image_bytes: &[u8]) -> Result<String> {
        self.recognize_region(image_bytes, RegionType::FullPage)
            .await
    }

    pub async fn recognize_region(
        &self,
        image_bytes: &[u8],
        region_type: RegionType,
    ) -> Result<String> {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image_bytes);
        let image_url = format!("data:image/jpeg;base64,{}", b64);

        let body = build_request_body(&self.model, region_type, &image_url);

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let json: serde_json::Value = resp.json().await?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}

/// Sampling parameters tuned for verbatim OCR on academic layouts.
/// See the project's analysis doc for the empirical safe band:
///   - temperature=0.0 + top_k=1: greedy argmax (vLLM resets top_p→1
///     and ignores top_k at T=0; setting both is honest about intent
///     and matches the Ollama backend on the same wire format).
///   - top_p=1.0: vLLM's _verify_greedy_sampling resets this anyway.
///   - repetition_penalty=1.10: safe band per Wang/Zou/Min 2025 and
///     Holtzman 2019. Above 1.15 substitutes visually similar tokens
///     on numeric/tabular content (0→O, l→1).
///   - frequency_penalty=0.2: scales with token count; counteracts the
///     logarithmic self-reinforcement of induction-head copying. Only
///     non-zero penalty in this stack that grows with repetition.
///   - presence_penalty=0.0: binary form, doesn't break growth.
///   - extra_body.no_repeat_ngram_size=12: large enough to allow
///     legitimate citation entries, small enough to catch loops.
///     vLLM-specific; non-vLLM OpenAI-compat servers ignore unknown
///     keys harmlessly.
/// Ollama's OpenAI-compat shim (if used) silently drops the penalty
/// params per ollama#14493 — same caveat as the dedicated Ollama backend.
fn build_request_body(model: &str, region_type: RegionType, image_url: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": { "url": image_url }
                    },
                    {
                        "type": "text",
                        "text": region_type.prompt()
                    }
                ]
            }
        ],
        "temperature": 0.0,
        "max_tokens": 8192,
        "top_k": 1,
        "top_p": 1.0,
        "repetition_penalty": 1.10,
        "frequency_penalty": 0.2,
        "presence_penalty": 0.0,
        "extra_body": { "no_repeat_ngram_size": 12 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_carries_anti_repetition_params() {
        // Regression guard for the sampling-param tuning. If you change a
        // value here, read the doc comment on build_request_body and the
        // project's analysis doc first — wrong values reintroduce repetition
        // loops on dense academic text or substitute numerals on tables.
        let body = build_request_body("glm-ocr", RegionType::Text, "data:image/jpeg;base64,Zm9v");
        assert_eq!(body["model"], "glm-ocr");
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["top_k"], 1);
        assert_eq!(body["top_p"], 1.0);
        assert_eq!(body["repetition_penalty"], 1.10);
        assert_eq!(body["frequency_penalty"], 0.2);
        assert_eq!(body["presence_penalty"], 0.0);
        assert_eq!(body["extra_body"]["no_repeat_ngram_size"], 12);
    }

    #[test]
    fn request_body_embeds_image_url_and_prompt() {
        let body = build_request_body("m", RegionType::Table, "data:image/jpeg;base64,YmFy");
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/jpeg;base64,YmFy"
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], RegionType::Table.prompt());
    }
}
