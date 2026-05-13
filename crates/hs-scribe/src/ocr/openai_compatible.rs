use super::region::RegionType;
use super::repetition_detector::{RepetitionDetector, RepetitionLoopError};
use super::sse_buffer::SseBuffer;
use anyhow::Result;
use futures_util::StreamExt;
use std::error::Error as _;
use std::time::Duration;

/// Backoff schedule for retriable VLM transport errors. The total worst-case
/// added latency per region is ~4.2 s, which fits under the per-page scribe
/// budget. Three attempts catches transient `--parallel` slot overflows on
/// llama-server (which manifest as TCP RST / `ConnectionReset`) without
/// queuing forever when the backend is actually down.
const RETRY_BACKOFFS: &[Duration] = &[
    Duration::from_millis(200),
    Duration::from_millis(800),
    Duration::from_millis(3200),
];

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

        let resp = self.send_with_retry(&body).await?;

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

    /// POST the chat-completions request with bounded exponential backoff
    /// on retriable transport-class failures. Retries kick in when the
    /// VLM backend's listen backlog is full (TCP RST), the service is
    /// briefly down (refused/closed), or it returns a transient 5xx —
    /// states that resolve within seconds once an in-flight slot frees up.
    /// 4xx, successful streams, and the mid-stream repetition detector
    /// remain paper-fatal: those signal real problems with the request
    /// or content, not transient backend pressure.
    async fn send_with_retry(&self, body: &serde_json::Value) -> Result<reqwest::Response> {
        let endpoint = format!("{}/v1/chat/completions", self.url);
        let mut attempt = 0usize;
        loop {
            let send_res = self.client.post(&endpoint).json(body).send().await;
            match send_res {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    let retriable_5xx = matches!(status.as_u16(), 502..=504);
                    if retriable_5xx && attempt < RETRY_BACKOFFS.len() {
                        let delay = jittered(RETRY_BACKOFFS[attempt]);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max = RETRY_BACKOFFS.len(),
                            status = %status,
                            delay_ms = delay.as_millis() as u64,
                            "VLM transient 5xx; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Ok(resp.error_for_status()?);
                }
                Err(err) => {
                    if is_retriable_transport(&err) && attempt < RETRY_BACKOFFS.len() {
                        let delay = jittered(RETRY_BACKOFFS[attempt]);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max = RETRY_BACKOFFS.len(),
                            error = %err,
                            delay_ms = delay.as_millis() as u64,
                            "VLM transport error; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(err.into());
                }
            }
        }
    }
}

/// Inspect a `reqwest::Error` chain for io-layer kinds that indicate the
/// VLM backend is momentarily unable to accept the request, not that the
/// request itself is malformed. Connect, request-builder I/O (TCP RST
/// during write), timeout, and broken-pipe all qualify.
fn is_retriable_transport(err: &reqwest::Error) -> bool {
    if err.is_connect() || err.is_timeout() {
        return true;
    }
    let mut src: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(e) = src {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            return matches!(
                io.kind(),
                ConnectionReset
                    | ConnectionAborted
                    | ConnectionRefused
                    | BrokenPipe
                    | TimedOut
                    | UnexpectedEof
            );
        }
        src = e.source();
    }
    false
}

/// Add up to 50 ms of pseudo-jitter to a backoff delay. Avoids
/// synchronised retries from a fan-out of region calls all hitting the
/// same TCP RST at the same instant — a real failure mode at parallel=8
/// when a multi-page burst lands together. Cheap clock-based nondeterminism
/// is enough; no `rand` dep needed.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base + Duration::from_millis(nanos % 50)
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
///
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
    fn retry_budget_fits_under_per_page_scribe_budget() {
        // The per-page scribe budget is ~10 s (worst case Metal at 1800 px,
        // see `crates/hs-scribe/src/config.rs:303`). Three retries with the
        // current schedule add at most ~4.25 s of pure wait time per region
        // (200 + 800 + 3200 ms + up to 3 × 50 ms jitter). Anyone bumping the
        // schedule needs to keep the per-page budget intact; this guard
        // makes the constraint visible at edit time.
        let total: u64 = RETRY_BACKOFFS.iter().map(|d| d.as_millis() as u64).sum();
        let jitter_ceiling = (RETRY_BACKOFFS.len() as u64) * 50;
        assert!(
            total + jitter_ceiling <= 5_000,
            "retry budget {} ms + jitter {} ms blows the per-page scribe budget",
            total,
            jitter_ceiling
        );
    }

    #[test]
    fn jitter_stays_within_50_ms() {
        let base = Duration::from_millis(200);
        for _ in 0..32 {
            let j = jittered(base);
            let added = j.checked_sub(base).expect("jitter must not shrink base");
            assert!(
                added <= Duration::from_millis(50),
                "jitter exceeded 50 ms: {:?}",
                added
            );
        }
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
