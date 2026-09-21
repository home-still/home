use super::region::RegionType;
use super::repetition_detector::{RepetitionDetector, RepetitionLoopError};
use super::sse_buffer::SseBuffer;
use anyhow::Result;
use futures_util::StreamExt;

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

    /// Streamed VLM call with mid-stream repetition-loop detection. The
    /// request goes out with `stream: true`; deltas are accumulated into
    /// `output` and fed into a [`RepetitionDetector`]. When the detector
    /// fires we drop the response stream (closes the TCP connection,
    /// llama.cpp's `llama-server` checks per-token and stops generation
    /// within ~50 ms) and return a [`RepetitionLoopError`] carrying the
    /// partial output. The caller treats that as a per-region failure
    /// and the page assembles markdown from surviving regions.
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

        let mut stream = resp.bytes_stream();
        let mut sse = SseBuffer::new();
        let mut detector = RepetitionDetector::default();
        let mut output = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            for event in sse.feed(&bytes) {
                if event == "[DONE]" {
                    return Ok(output);
                }
                let Some(delta) = parse_delta_content(&event) else {
                    continue;
                };
                if delta.is_empty() {
                    continue;
                }
                output.push_str(&delta);
                detector.feed(&delta);
                if let Some(reason) = detector.check() {
                    let bytes_at_abort = output.len();
                    // Dropping the stream closes the underlying reqwest
                    // body, which signals the server to stop generation.
                    drop(stream);
                    return Err(anyhow::Error::new(RepetitionLoopError {
                        reason,
                        partial_output: output,
                        bytes_at_abort,
                    }));
                }
            }
        }
        // Stream ended without `[DONE]`. llama-server emits `[DONE]`
        // reliably on graceful completion; we got here either because
        // the connection dropped mid-stream (treat as transport error)
        // or the server closed without a sentinel (some non-llama.cpp
        // backends). Return what we have rather than fail — the
        // postprocess QC gate will catch a truncated output as low
        // quality if it's actually broken.
        Ok(output)
    }
}

/// Extract `choices[0].delta.content` from a streamed event payload.
/// Returns `None` for events that don't carry a content delta (e.g. the
/// initial `role` event, the final usage event with `stream_options`).
fn parse_delta_content(event_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(event_json).ok()?;
    v["choices"][0]["delta"]["content"]
        .as_str()
        .map(|s| s.to_string())
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
        "extra_body": { "no_repeat_ngram_size": 12 },
        // Streaming enables per-delta repetition-loop detection. When the
        // RepetitionDetector fires we drop the response body, which closes
        // the TCP connection — llama.cpp's `llama-server` polls the
        // connection state per-token and stops generation within ~50 ms.
        // include_usage is harmless on llama-server; vLLM uses it to emit
        // a final usage event we ignore.
        "stream": true,
        "stream_options": { "include_usage": true }
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
    fn request_body_carries_stream_flags() {
        // The whole point of v1 is streaming + mid-stream loop abort.
        // Regression guard: dropping `stream: true` silently reverts
        // every call to non-streaming, defeating the detector entirely.
        let body = build_request_body("glm-ocr", RegionType::Text, "data:image/jpeg;base64,Zm9v");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn parse_delta_content_extracts_streaming_chunk() {
        let event = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_delta_content(event), Some("Hello".to_string()));
    }

    #[test]
    fn parse_delta_content_returns_none_for_role_only_event() {
        // First event in a stream typically carries only role, no content.
        let event = r#"{"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert_eq!(parse_delta_content(event), None);
    }

    #[test]
    fn parse_delta_content_returns_none_for_final_usage_event() {
        // With stream_options.include_usage=true, the final event has
        // empty choices and a usage block we don't care about.
        let event = r#"{"choices":[],"usage":{"prompt_tokens":42}}"#;
        assert_eq!(parse_delta_content(event), None);
    }

    #[test]
    fn parse_delta_content_returns_none_for_malformed_json() {
        let event = "not json";
        assert_eq!(parse_delta_content(event), None);
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
