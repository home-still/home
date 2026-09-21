//! Server-Sent Events line buffer.
//!
//! HTTP streaming bodies arrive as arbitrary byte chunks — TCP packet
//! boundaries cut SSE lines mid-character with no regard for `\n` or
//! `data: ` framing. The OpenAI streaming format is one event per
//! `data: <json>\n\n` block; the terminator is `data: [DONE]\n\n`. To
//! consume a stream we accumulate raw bytes across `bytes_stream()` polls
//! and only yield an event once we've seen its terminating blank line.
//!
//! Pure sync; no I/O. The streaming loop in `openai_compatible.rs` calls
//! [`SseBuffer::feed`] with each `Bytes` chunk and iterates the returned
//! events. ~40 LoC + tests; no external SSE crate needed.

/// Accumulator that yields complete SSE events as bytes arrive.
///
/// One event is everything between two consecutive blank lines (`\n\n`).
/// Lines starting with `data: ` carry the payload; other lines (`event:`,
/// `id:`, `retry:`) are ignored — OpenAI's streaming endpoint only uses
/// `data:` and the OCR backend doesn't need anything else.
pub struct SseBuffer {
    /// Bytes received so far that haven't yet completed an event.
    buf: Vec<u8>,
}

impl Default for SseBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SseBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    /// Feed a chunk of bytes from the wire. Returns the SSE `data:` payload
    /// strings extracted so far. Each returned string is a complete event
    /// payload (the part after `data: `). The terminating `[DONE]` sentinel
    /// is yielded as the literal string `"[DONE]"` so the caller can stop
    /// iterating.
    ///
    /// This implementation accepts both `\n\n` and `\r\n\r\n` line
    /// terminators. llama.cpp's `llama-server` uses `\n\n`; vLLM also
    /// uses `\n\n`; some hosted OpenAI-compat proxies (CloudFront in
    /// front of a self-hosted gateway) normalize to `\r\n\r\n`. Cheap to
    /// handle both rather than discover the wrong assumption in prod.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);

        let mut out = Vec::new();
        while let Some((event_end, sep_len)) = find_event_boundary(&self.buf) {
            // Drain the event from the buffer: bytes [0..event_end) are
            // the event body; [event_end..event_end+sep_len) is the
            // blank-line separator we discard.
            let event_bytes: Vec<u8> = self.buf.drain(..event_end + sep_len).collect();
            // Only the separator was drained from the trailing slice.
            // Parse the event body for `data:` lines.
            if let Some(payload) = parse_data_lines(&event_bytes[..event_end]) {
                out.push(payload);
            }
        }
        out
    }
}

/// Find the first `\n\n` or `\r\n\r\n` separator. Returns `(offset_of_first_newline,
/// separator_length)` so the caller can split body from separator.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    // Scan for either pattern; pick the earlier hit. Search up to len-1
    // for `\n\n` and len-3 for `\r\n\r\n`.
    let crlf_idx = (0..buf.len().saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n");
    let lf_idx = (0..buf.len().saturating_sub(1)).find(|&i| &buf[i..i + 2] == b"\n\n");

    match (crlf_idx, lf_idx) {
        (Some(c), Some(l)) if c < l => Some((c, 4)),
        (Some(_), Some(l)) => {
            // CR-LF pattern is later, but LF-LF still cuts cleanly. Prefer LF-LF.
            Some((l, 2))
        }
        (Some(c), None) => Some((c, 4)),
        (None, Some(l)) => Some((l, 2)),
        (None, None) => None,
    }
}

/// Extract the `data:` payload from a single SSE event body. The event may
/// contain multiple `data:` lines for one logical message — concatenate them
/// with `\n` per the SSE spec. Lines that are blank, comments (`:`), or
/// non-`data:` directives are skipped.
fn parse_data_lines(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let mut payloads: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        // Tolerate a trailing CR from CRLF-terminated streams.
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data: ") {
            payloads.push(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            // Some servers emit `data:foo` without the post-colon space.
            payloads.push(rest);
        }
    }
    if payloads.is_empty() {
        return None;
    }
    Some(payloads.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_event_in_one_chunk() {
        let mut b = SseBuffer::new();
        let events = b.feed(b"data: hello\n\n");
        assert_eq!(events, vec!["hello"]);
    }

    #[test]
    fn event_split_across_chunks_yields_once() {
        let mut b = SseBuffer::new();
        let part1 = b.feed(b"data: hel");
        let part2 = b.feed(b"lo\n\n");
        assert!(part1.is_empty());
        assert_eq!(part2, vec!["hello"]);
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut b = SseBuffer::new();
        let events = b.feed(b"data: alpha\n\ndata: beta\n\ndata: gamma\n\n");
        assert_eq!(events, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn separator_split_across_chunks() {
        let mut b = SseBuffer::new();
        // Send the body but end before the separator.
        let p1 = b.feed(b"data: hello\n");
        assert!(p1.is_empty(), "incomplete separator must not yield");
        let p2 = b.feed(b"\n");
        assert_eq!(p2, vec!["hello"]);
    }

    #[test]
    fn done_sentinel_passes_through_verbatim() {
        let mut b = SseBuffer::new();
        let events = b.feed(b"data: [DONE]\n\n");
        assert_eq!(events, vec!["[DONE]"]);
    }

    #[test]
    fn comments_and_other_directives_are_skipped() {
        let mut b = SseBuffer::new();
        let events = b.feed(b": keepalive comment\nevent: ping\nid: 42\ndata: real\n\n");
        assert_eq!(events, vec!["real"]);
    }

    #[test]
    fn crlf_terminators_are_accepted() {
        let mut b = SseBuffer::new();
        let events = b.feed(b"data: hello\r\n\r\n");
        assert_eq!(events, vec!["hello"]);
    }

    #[test]
    fn data_without_post_colon_space_is_accepted() {
        // Some non-OpenAI servers emit `data:value` without the space.
        let mut b = SseBuffer::new();
        let events = b.feed(b"data:hello\n\n");
        assert_eq!(events, vec!["hello"]);
    }

    #[test]
    fn multi_data_line_payload_joins_with_newline() {
        // SSE spec: multiple `data:` lines in one event concatenate
        // with newline as the logical message.
        let mut b = SseBuffer::new();
        let events = b.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(events, vec!["line1\nline2"]);
    }

    #[test]
    fn invalid_utf8_event_is_dropped_not_crashed() {
        let mut b = SseBuffer::new();
        // 0xff is not valid UTF-8 in any position — entire event is
        // skipped, not panicked.
        let events = b.feed(b"data: \xff\xfe\xfd\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn byte_stream_simulating_real_chat_completions() {
        // Realistic-ish: openai/llama-server emits one delta per chunk,
        // sometimes multiple deltas batched, with the [DONE] sentinel
        // last. Simulate small TCP-window splits.
        let mut b = SseBuffer::new();
        let mut all = Vec::new();
        all.extend(b.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n"));
        all.extend(b.feed(b"\ndata: {\"choices"));
        all.extend(b.feed(b"\":[{\"delta\":{\"content\":\"llo\"}}]}\n\ndata: [DONE]"));
        all.extend(b.feed(b"\n\n"));
        assert_eq!(all.len(), 3);
        assert!(all[0].contains("\"He\""));
        assert!(all[1].contains("\"llo\""));
        assert_eq!(all[2], "[DONE]");
    }
}
