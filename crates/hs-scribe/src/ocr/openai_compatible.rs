use super::region::RegionType;
use super::repetition_detector::{RepetitionDetector, RepetitionLoopError};
use super::sse_buffer::SseBuffer;
use crate::classify::{ConvertFailure, FailureCode};
use anyhow::{Context, Result};
use futures_util::{Stream, StreamExt};
use std::error::Error as _;
use std::time::Duration;

/// How long a connection attempt to the VLM backend may take. The backend
/// is on the LAN or loopback; a connect that takes longer is a dead host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Bearer token for auth-gated backends (e.g. `llama-swap`). `None`
    /// for a bare `llama-server` that serves without auth — in which case
    /// no `Authorization` header is sent at all.
    api_key: Option<String>,
}

impl OpenAiBackend {
    /// `idle_timeout` bounds the silence on the connection: the wait for the
    /// first byte (a cold model load or a prompt-cache rebuild can take a
    /// minute or more) and the gap between any two reads of the stream. A
    /// stalled backend therefore fails the region instead of pinning a VLM
    /// permit until the whole-convert deadline. There is deliberately no
    /// overall timeout: a long generation that keeps producing tokens is
    /// healthy.
    pub fn new(
        url: &str,
        model: &str,
        api_key: Option<String>,
        idle_timeout: Duration,
    ) -> Result<Self> {
        let client = hs_common::http::client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(idle_timeout)
            .build()
            .context("failed to build the VLM HTTP client")?;
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_string(),
            model: model.strip_suffix(":latest").unwrap_or(model).to_string(),
            api_key: api_key.filter(|k| !k.is_empty()),
        })
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

        read_completion(resp.bytes_stream()).await
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
            let mut req = self.client.post(&endpoint).json(body);
            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }
            let send_res = req.send().await;
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

fn transport_failure(message: impl Into<String>) -> anyhow::Error {
    ConvertFailure::err(FailureCode::VlmTransportError, message)
}

/// One decoded SSE event of a chat-completions stream.
#[derive(Debug, PartialEq)]
enum StreamEvent {
    /// The `[DONE]` sentinel.
    Done,
    /// The backend reported an error inside the stream (`{"error": ...}`),
    /// which it can do after the response has already started with 200.
    BackendError(String),
    /// `choices[0].delta.content` and `choices[0].finish_reason`, either of
    /// which may be absent (the initial `role` event, the final usage event
    /// with `stream_options`).
    Chunk {
        delta: Option<String>,
        finish: Option<String>,
    },
}

/// Decode one SSE event payload. `Err` means the payload is not a valid
/// chat-completions event at all.
fn parse_event(event: &str) -> Result<StreamEvent, String> {
    if event == "[DONE]" {
        return Ok(StreamEvent::Done);
    }
    let v: serde_json::Value = serde_json::from_str(event).map_err(|e| {
        let head: String = event.chars().take(200).collect();
        format!("VLM stream event is not JSON ({e}): {head}")
    })?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Ok(StreamEvent::BackendError(message));
    }
    let choice = &v["choices"][0];
    Ok(StreamEvent::Chunk {
        delta: choice["delta"]["content"].as_str().map(str::to_string),
        finish: choice["finish_reason"].as_str().map(str::to_string),
    })
}

/// Consume a chat-completions SSE body into the generated text.
///
/// A completion is complete only when the backend says so: the `[DONE]`
/// sentinel, or `finish_reason == "stop"` (some servers omit the sentinel).
/// Anything else that ends the stream is a failure, never a short success:
/// a clean EOF without either (the connection died, or the server gave up),
/// `finish_reason == "length"` (the token limit cut the answer off), an
/// `{"error": ...}` event, an event that is not valid UTF-8 or JSON, and a
/// read error on the body. Whatever the cause, the region's text is
/// incomplete and must not be stamped as a conversion. The streaming
/// repetition detector aborts with a [`RepetitionLoopError`] holding the
/// partial output; dropping `stream` closes the connection so the backend
/// stops generating.
async fn read_completion<S, E>(mut stream: S) -> Result<String>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut sse = SseBuffer::new();
    let mut detector = RepetitionDetector::default();
    let mut output = String::new();
    let mut finished = false;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| {
            transport_failure(format!(
                "VLM stream failed after {} bytes of output: {e}",
                output.len()
            ))
        })?;
        for event in sse
            .feed(&bytes)
            .map_err(|e| transport_failure(e.to_string()))?
        {
            match parse_event(&event).map_err(transport_failure)? {
                StreamEvent::Done => return Ok(output),
                StreamEvent::BackendError(message) => {
                    return Err(transport_failure(format!(
                        "VLM backend reported an error mid-stream after {} bytes of output: {message}",
                        output.len()
                    )));
                }
                StreamEvent::Chunk { delta, finish } => {
                    if let Some(delta) = delta.filter(|d| !d.is_empty()) {
                        output.push_str(&delta);
                        detector.feed(&delta);
                        if let Some(reason) = detector.check() {
                            let bytes_at_abort = output.len();
                            return Err(anyhow::Error::new(RepetitionLoopError {
                                reason,
                                partial_output: output,
                                bytes_at_abort,
                            }));
                        }
                    }
                    match finish.as_deref() {
                        None => {}
                        Some("stop") => finished = true,
                        Some("length") => {
                            return Err(ConvertFailure::err(
                                FailureCode::VlmOutputTruncated,
                                format!(
                                    "VLM hit its token limit (finish_reason=length) after {} bytes of output",
                                    output.len()
                                ),
                            ));
                        }
                        Some(other) => {
                            return Err(transport_failure(format!(
                                "VLM stopped abnormally (finish_reason={other}) after {} bytes of output",
                                output.len()
                            )));
                        }
                    }
                }
            }
        }
    }
    if finished {
        Ok(output)
    } else {
        Err(transport_failure(format!(
            "VLM stream ended after {} bytes of output without [DONE] or finish_reason=stop",
            output.len()
        )))
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

    // ── stream completion ──────────────────────────────────────────────

    fn sse_body(parts: &[&str]) -> Vec<std::result::Result<bytes::Bytes, std::io::Error>> {
        parts
            .iter()
            .map(|p| Ok(bytes::Bytes::from(p.as_bytes().to_vec())))
            .collect()
    }

    fn delta(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": text}}]})
        )
    }

    fn finish(reason: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]})
        )
    }

    async fn run(parts: &[&str]) -> Result<String> {
        read_completion(futures_util::stream::iter(sse_body(parts))).await
    }

    fn code_of(err: &anyhow::Error) -> Option<FailureCode> {
        crate::classify::failure_code(err)
    }

    #[tokio::test]
    async fn a_stream_closed_by_done_returns_the_text() {
        let (a, b) = (delta("Hello, "), delta("world"));
        let out = run(&[&a, &b, &finish("stop"), "data: [DONE]\n\n"])
            .await
            .unwrap();
        assert_eq!(out, "Hello, world");
    }

    #[tokio::test]
    async fn finish_reason_stop_completes_a_stream_that_omits_done() {
        let a = delta("text");
        let out = run(&[&a, &finish("stop")]).await.unwrap();
        assert_eq!(out, "text");
    }

    #[tokio::test]
    async fn done_alone_completes_a_stream_that_never_sent_a_finish_reason() {
        let a = delta("text");
        assert_eq!(run(&[&a, "data: [DONE]\n\n"]).await.unwrap(), "text");
    }

    #[tokio::test]
    async fn events_split_across_chunks_are_reassembled() {
        let full = format!("{}{}data: [DONE]\n\n", delta("Hel"), delta("lo"));
        let (head, tail) = full.split_at(17);
        assert_eq!(run(&[head, tail]).await.unwrap(), "Hello");
    }

    #[tokio::test]
    async fn a_clean_eof_without_done_or_stop_is_a_failure_not_a_short_success() {
        let a = delta("only half of the pa");
        let err = run(&[&a]).await.unwrap_err();
        assert_eq!(
            code_of(&err),
            Some(FailureCode::VlmTransportError),
            "{err:#}"
        );
        let err = run(&[]).await.unwrap_err();
        assert_eq!(code_of(&err), Some(FailureCode::VlmTransportError));
    }

    #[tokio::test]
    async fn hitting_the_token_limit_is_a_failure() {
        let a = delta("a very long answer that was cut");
        let err = run(&[&a, &finish("length"), "data: [DONE]\n\n"])
            .await
            .unwrap_err();
        assert_eq!(
            code_of(&err),
            Some(FailureCode::VlmOutputTruncated),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn any_other_finish_reason_is_a_failure() {
        let err = run(&[&finish("content_filter")]).await.unwrap_err();
        assert_eq!(code_of(&err), Some(FailureCode::VlmTransportError));
    }

    #[tokio::test]
    async fn an_error_event_inside_the_stream_is_a_failure() {
        let a = delta("partial");
        let boom =
            "data: {\"error\":{\"message\":\"CUDA out of memory\",\"type\":\"server_error\"}}\n\n";
        let err = run(&[&a, boom, "data: [DONE]\n\n"]).await.unwrap_err();
        assert_eq!(code_of(&err), Some(FailureCode::VlmTransportError));
        assert!(format!("{err:#}").contains("CUDA out of memory"), "{err:#}");
    }

    #[tokio::test]
    async fn an_event_that_is_not_valid_utf8_is_a_failure_not_a_dropped_event() {
        let a = delta("before");
        let err = read_completion(futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(bytes::Bytes::from(a.into_bytes())),
            Ok(bytes::Bytes::from_static(b"data: \xff\xfe\n\n")),
            Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n")),
        ]))
        .await
        .unwrap_err();
        assert_eq!(
            code_of(&err),
            Some(FailureCode::VlmTransportError),
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("UTF-8"), "{err:#}");
    }

    #[tokio::test]
    async fn an_event_that_is_not_json_is_a_failure() {
        let err = run(&["data: this is not json\n\n", "data: [DONE]\n\n"])
            .await
            .unwrap_err();
        assert_eq!(code_of(&err), Some(FailureCode::VlmTransportError));
    }

    #[tokio::test]
    async fn a_body_read_error_mid_stream_is_a_failure() {
        let a = delta("text");
        let err = read_completion(futures_util::stream::iter(vec![
            Ok(bytes::Bytes::from(a.into_bytes())),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection closed before message completed",
            )),
        ]))
        .await
        .unwrap_err();
        assert_eq!(code_of(&err), Some(FailureCode::VlmTransportError));
    }

    #[tokio::test]
    async fn a_repetition_loop_aborts_with_the_partial_output() {
        let looped = "and relationship ".repeat(40);
        let a = delta(&looped);
        let err = run(&[&a, "data: [DONE]\n\n"]).await.unwrap_err();
        let loop_err = err
            .downcast_ref::<RepetitionLoopError>()
            .expect("a loop is reported as the controlled abort, not a transport failure");
        assert!(loop_err.bytes_at_abort > 0);
        assert_eq!(code_of(&err), None);
    }

    #[test]
    fn parse_event_reads_content_and_finish_reason() {
        assert_eq!(
            parse_event(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#),
            Ok(StreamEvent::Chunk {
                delta: Some("Hello".into()),
                finish: None
            })
        );
        assert_eq!(
            parse_event(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            Ok(StreamEvent::Chunk {
                delta: None,
                finish: Some("stop".into())
            })
        );
    }

    #[test]
    fn role_only_and_usage_events_carry_nothing() {
        let nothing = StreamEvent::Chunk {
            delta: None,
            finish: None,
        };
        // First event in a stream typically carries only role, no content.
        assert_eq!(
            parse_event(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
            Ok(StreamEvent::Chunk {
                delta: None,
                finish: None
            })
        );
        // With stream_options.include_usage=true, the final event has
        // empty choices and a usage block we don't care about.
        assert_eq!(
            parse_event(r#"{"choices":[],"usage":{"prompt_tokens":42}}"#),
            Ok(nothing)
        );
    }

    #[test]
    fn parse_event_rejects_malformed_payloads_and_reads_error_events() {
        assert!(parse_event("not json").is_err());
        assert_eq!(parse_event("[DONE]"), Ok(StreamEvent::Done));
        assert_eq!(
            parse_event(r#"{"error":"plain string error"}"#),
            Ok(StreamEvent::BackendError("\"plain string error\"".into()))
        );
        assert_eq!(
            parse_event(r#"{"error":null,"choices":[]}"#),
            Ok(StreamEvent::Chunk {
                delta: None,
                finish: None
            })
        );
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
