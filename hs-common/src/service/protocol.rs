use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Generic NDJSON stream line. Each service provides its own progress and result types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamLine<P, R> {
    Progress(P),
    Result(R),
    Error(String),
}

/// Extract readiness info for pool-based server selection.
pub trait ReadinessInfo {
    fn is_ready(&self) -> bool;
    fn available_slots(&self) -> usize;
    /// Total advertised slot capacity (busy + free), used at consumer
    /// startup to size the in-flight semaphore for heterogeneous fleets.
    /// Defaults to [`Self::available_slots`] so single-slot services
    /// (distill, mocks) need no change.
    fn total_slots(&self) -> usize {
        self.available_slots()
    }
    /// False when the host answered but refuses all work at an admission
    /// gate, as opposed to being busy. Zero available slots alone cannot
    /// tell the two apart. Defaults to true for services without a gate.
    fn admits_work(&self) -> bool {
        true
    }
}

/// Common service client interface for health/readiness checks.
#[async_trait]
pub trait ServiceClient: Send + Sync {
    type Health: DeserializeOwned;
    type Readiness: DeserializeOwned + ReadinessInfo;

    fn url(&self) -> &str;
    async fn health(&self) -> Result<Self::Health>;
    async fn readiness(&self) -> Result<Self::Readiness>;
}

/// Longest single NDJSON line accepted. A final `Result` line carries a whole
/// converted document, so this is generous; it only stops a peer that never
/// sends a newline from growing the buffer without bound.
const MAX_LINE_BYTES: usize = 256 * 1024 * 1024;

/// Incremental splitter for newline-delimited records. Each byte is
/// examined for `\n` once, however many chunks a line arrives in: a
/// multi-megabyte `Result` line delivered in 16 KiB chunks costs O(n), not
/// O(n²) from rescanning the buffer on every chunk.
#[derive(Debug, Default)]
pub struct NdjsonSplitter {
    buf: Vec<u8>,
    /// Start of the first unconsumed byte in `buf`.
    start: usize,
    /// Everything in `buf[start..scanned]` is known to contain no `\n`.
    scanned: usize,
}

impl NdjsonSplitter {
    /// Append bytes received from the peer.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.start > 0 {
            // Drop the consumed prefix; what remains is a partial line.
            self.buf.drain(..self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete, non-empty line (surrounding whitespace trimmed),
    /// or `None` when more bytes are needed. A line that is not valid UTF-8
    /// or exceeds [`MAX_LINE_BYTES`] is an error, not something to skip.
    pub fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            let from = self.scanned.max(self.start);
            let Some(rel) = self.buf[from..].iter().position(|&b| b == b'\n') else {
                self.scanned = self.buf.len();
                anyhow::ensure!(
                    self.buf.len() - self.start <= MAX_LINE_BYTES,
                    "NDJSON line exceeds {MAX_LINE_BYTES} bytes without a newline"
                );
                return Ok(None);
            };
            let end = from + rel;
            let raw = &self.buf[self.start..end];
            self.start = end + 1;
            self.scanned = self.start;
            if let Some(line) = Self::decode(raw)? {
                return Ok(Some(line));
            }
        }
    }

    /// At end of stream: whatever is left after the last newline, as a final
    /// line. A truncated final line is then reported by the parser instead of
    /// being silently dropped.
    pub fn finish(&mut self) -> Result<Option<String>> {
        let raw = &self.buf[self.start..];
        let out = Self::decode(raw)?;
        self.buf.clear();
        self.start = 0;
        self.scanned = 0;
        Ok(out)
    }

    fn decode(raw: &[u8]) -> Result<Option<String>> {
        let text = std::str::from_utf8(raw).context("NDJSON line is not valid UTF-8")?;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }
}

/// Interpret one NDJSON line. `Ok(Some(result))` ends the stream. A line
/// that does not parse is an error: a stream we cannot understand is a
/// protocol mismatch (or corruption), and skipping it would hide a dropped
/// progress or result record.
fn handle_line<P, R>(line: &str, on_progress: &impl Fn(P)) -> Result<Option<R>>
where
    P: DeserializeOwned,
    R: DeserializeOwned,
{
    match serde_json::from_str::<StreamLine<P, R>>(line) {
        Ok(StreamLine::Progress(event)) => {
            on_progress(event);
            Ok(None)
        }
        Ok(StreamLine::Result(result)) => Ok(Some(result)),
        Ok(StreamLine::Error(msg)) => anyhow::bail!("Server error: {msg}"),
        Err(e) => {
            let head: String = line.chars().take(200).collect();
            anyhow::bail!("malformed stream line ({e}): {head}")
        }
    }
}

/// Helper to parse an NDJSON stream from a reqwest response.
/// Calls `on_progress` for each progress event and returns the final result.
/// The one NDJSON reader in the workspace (distill and scribe clients both
/// use it).
pub async fn read_ndjson_stream<P, R>(
    mut resp: reqwest::Response,
    on_progress: impl Fn(P),
) -> Result<R>
where
    P: DeserializeOwned,
    R: DeserializeOwned,
{
    let mut lines = NdjsonSplitter::default();
    while let Some(bytes) = resp.chunk().await.context("Stream read error")? {
        lines.push(&bytes);
        while let Some(line) = lines.next_line()? {
            if let Some(result) = handle_line::<P, R>(&line, &on_progress)? {
                return Ok(result);
            }
        }
    }
    if let Some(line) = lines.finish()? {
        if let Some(result) = handle_line::<P, R>(&line, &on_progress)? {
            return Ok(result);
        }
    }
    anyhow::bail!("Server closed connection without sending result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn drain(s: &mut NdjsonSplitter) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(l) = s.next_line().unwrap() {
            out.push(l);
        }
        out
    }

    #[test]
    fn splits_lines_across_arbitrary_chunk_boundaries() {
        let mut s = NdjsonSplitter::default();
        let mut got = Vec::new();
        for chunk in ["{\"a\":1}\n{\"b\"", ":2}\n\n  {\"c\":3}", "  \n"] {
            s.push(chunk.as_bytes());
            got.extend(drain(&mut s));
        }
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]);
        assert_eq!(s.finish().unwrap(), None);
    }

    #[test]
    fn multibyte_characters_split_mid_sequence_still_decode() {
        let line = "{\"m\":\"Müller 中文\"}\n".as_bytes();
        let mut s = NdjsonSplitter::default();
        let mut got = Vec::new();
        // One byte at a time splits every multi-byte sequence.
        for b in line {
            s.push(std::slice::from_ref(b));
            got.extend(drain(&mut s));
        }
        assert_eq!(got, vec!["{\"m\":\"Müller 中文\"}"]);
    }

    #[test]
    fn finish_returns_an_unterminated_final_line() {
        let mut s = NdjsonSplitter::default();
        s.push(b"{\"x\":1}\n{\"y\":2}");
        assert_eq!(drain(&mut s), vec!["{\"x\":1}"]);
        assert_eq!(s.finish().unwrap().as_deref(), Some("{\"y\":2}"));
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_lossy_line() {
        let mut s = NdjsonSplitter::default();
        s.push(&[b'{', 0xff, 0xfe, b'}', b'\n']);
        assert!(s.next_line().is_err());
    }

    /// RA-85: the old reader rescanned the whole buffer for `\n` on every
    /// chunk, so one 8 MB line in 1 KB chunks cost ~32 G byte comparisons.
    #[test]
    fn a_huge_line_in_small_chunks_is_scanned_once() {
        let mut line = vec![b'x'; 8 * 1024 * 1024];
        line.push(b'\n');
        let mut s = NdjsonSplitter::default();
        let started = std::time::Instant::now();
        let mut got = None;
        for chunk in line.chunks(1024) {
            s.push(chunk);
            if let Some(l) = s.next_line().unwrap() {
                got = Some(l);
            }
        }
        assert_eq!(got.map(|l| l.len()), Some(8 * 1024 * 1024));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "quadratic rescans: {:?}",
            started.elapsed()
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Prog {
        n: u32,
    }
    #[derive(Debug, Deserialize, PartialEq)]
    struct Res {
        done: bool,
    }

    #[test]
    fn handle_line_dispatches_progress_result_and_error() {
        let seen = RefCell::new(Vec::new());
        let cb = |p: Prog| seen.borrow_mut().push(p.n);
        assert_eq!(
            handle_line::<Prog, Res>(r#"{"progress":{"n":7}}"#, &cb).unwrap(),
            None
        );
        assert_eq!(*seen.borrow(), vec![7]);
        assert_eq!(
            handle_line::<Prog, Res>(r#"{"result":{"done":true}}"#, &cb).unwrap(),
            Some(Res { done: true })
        );
        let err = handle_line::<Prog, Res>(r#"{"error":"boom"}"#, &cb).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    /// RA-85: a line that does not parse used to be logged and skipped.
    #[test]
    fn malformed_line_is_an_error() {
        let cb = |_: Prog| {};
        for bad in ["not json", r#"{"progress":{"n":"x"}}"#, r#"{"mystery":1}"#] {
            let err = handle_line::<Prog, Res>(bad, &cb).unwrap_err();
            assert!(err.to_string().contains("malformed stream line"), "{err}");
        }
    }

    /// Serve one HTTP response whose chunked body is the given pieces.
    async fn serve_chunks(pieces: Vec<Vec<u8>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            for p in pieces {
                sock.write_all(format!("{:x}\r\n", p.len()).as_bytes())
                    .await
                    .unwrap();
                sock.write_all(&p).await.unwrap();
                sock.write_all(b"\r\n").await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            sock.write_all(b"0\r\n\r\n").await.unwrap();
        });
        format!("http://{addr}/")
    }

    async fn stream_from(pieces: Vec<Vec<u8>>) -> Result<Res> {
        let url = serve_chunks(pieces).await;
        let resp = reqwest::get(&url).await.unwrap();
        read_ndjson_stream::<Prog, Res>(resp, |_| {}).await
    }

    #[tokio::test]
    async fn reads_progress_then_result_split_mid_line() {
        let out = stream_from(vec![
            b"{\"progress\":{\"n\":1}}\n{\"resu".to_vec(),
            b"lt\":{\"done\":true}}\n".to_vec(),
        ])
        .await
        .unwrap();
        assert_eq!(out, Res { done: true });
    }

    #[tokio::test]
    async fn garbage_line_fails_the_stream_even_when_a_result_follows() {
        let err = stream_from(vec![
            b"garbage\n".to_vec(),
            b"{\"result\":{\"done\":true}}\n".to_vec(),
        ])
        .await
        .unwrap_err();
        assert!(err.to_string().contains("malformed stream line"), "{err}");
    }

    #[tokio::test]
    async fn stream_without_a_result_is_an_error() {
        let err = stream_from(vec![b"{\"progress\":{\"n\":1}}\n".to_vec()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("without sending result"), "{err}");
    }

    #[tokio::test]
    async fn unterminated_final_result_line_is_still_read() {
        let out = stream_from(vec![b"{\"result\":{\"done\":true}}".to_vec()])
            .await
            .unwrap();
        assert_eq!(out, Res { done: true });
    }

    #[tokio::test]
    async fn truncated_final_line_is_reported_not_dropped() {
        let err = stream_from(vec![b"{\"result\":{\"do".to_vec()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("malformed stream line"), "{err}");
    }
}
