//! Client-side PDF validation and page count.
//!
//! Dispatchers size the per-request convert timeout against a PDF's page
//! count, so a 500-page book gets a generous deadline while small papers
//! don't sit idle on the floor — and, because the same read runs before
//! anything is dispatched, a PDF that no backend can convert is refused at
//! the door instead of after an upload and a deadline's worth of waiting.
//!
//! There is no "unknown page count": every outcome is a count or an error,
//! and an error that is a verdict on the document is a typed
//! [`ConvertFailure`] (`classify` reads it out of the chain; an untyped
//! error is host state — libpdfium missing, the counter busy — and is
//! retried).
//!
//! The count comes from pdfium ([`crate::pdfium`]), the same parser that
//! renders the pages: no pre-scan guesses what a second parser would trip
//! over, and nothing here can overflow a stack on hostile nesting. Counting
//! is serialised through [`GATE`] (pdfium admits one caller at a time),
//! runs on the blocking pool, and is bounded in wall-clock time: a document
//! that keeps pdfium busy past [`COUNT_WORK_TIMEOUT`] is refused, and
//! callers queued behind a stuck parse give up after [`COUNT_QUEUE_TIMEOUT`]
//! with a retryable error rather than a false verdict on their own
//! document.

use crate::classify::{ConvertFailure, FailureCode};
use crate::pdfium::with_parser;
use anyhow::Result;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Largest PDF accepted anywhere in the pipeline: the scribe server's
/// upload limit, and the bound applied before pdfium sees the bytes.
pub const MAX_PDF_BYTES: usize = 256 * 1024 * 1024;

/// Pages the renderer can address (pdfium page indices are `u16`).
pub const MAX_PDF_PAGES: usize = u16::MAX as usize + 1;

/// Bytes inspected to tell what a non-PDF payload is.
pub const HEADER_PROBE_BYTES: usize = 4096;

/// How long one document may keep pdfium busy before it is refused. A
/// healthy count takes milliseconds; this only bounds a hostile one.
pub const COUNT_WORK_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a caller waits for its turn at pdfium. Longer than the work
/// timeout so that a single stuck document is reported to its own caller as
/// a refusal and to the callers behind it as "busy, retry".
pub const COUNT_QUEUE_TIMEOUT: Duration = Duration::from_secs(120);

/// One counting job at a time. A job holds its permit until pdfium returns
/// — also after its caller timed out — so an abandoned parse keeps
/// excluding the next one instead of piling up blocking threads.
static GATE: Semaphore = Semaphore::const_new(1);

/// Accept only bodies that start with `%PDF` (the header `PDF-1.x`
/// specifies exactly). Anything else will never convert — the VLM would
/// spend GPU time producing garbage — so say what it is instead:
/// paywall HTML renamed `.pdf` and truncated or binary downloads get
/// different codes.
pub fn check_header(head: &[u8]) -> Result<(), ConvertFailure> {
    let head = &head[..head.len().min(HEADER_PROBE_BYTES)];
    if head.starts_with(b"%PDF") {
        return Ok(());
    }
    Err(if hs_common::html::looks_like_html(head) {
        ConvertFailure::new(
            FailureCode::UnsupportedContentTypeHtml,
            "the object is HTML, not a PDF",
        )
    } else {
        ConvertFailure::new(
            FailureCode::UnsupportedContentTypeBinary,
            "the object does not start with %PDF",
        )
    })
}

fn too_big(len: u64) -> anyhow::Error {
    ConvertFailure::err(
        FailureCode::PdfParseError,
        format!("PDF is {len} bytes, over the {MAX_PDF_BYTES}-byte limit"),
    )
}

/// Number of pages in `bytes`, or why the PDF cannot be converted. Header
/// and size are checked first; pdfium reads the document from `bytes`
/// without copying it.
pub async fn count_pages(bytes: Bytes) -> Result<u32> {
    check_header(&bytes)?;
    if bytes.len() > MAX_PDF_BYTES {
        return Err(too_big(bytes.len() as u64));
    }
    bounded(
        &GATE,
        COUNT_QUEUE_TIMEOUT,
        COUNT_WORK_TIMEOUT,
        crate::pdfium::fault_wedged,
        move || with_parser(|parser| parser.count_pages_in_bytes(&bytes)),
    )
    .await
}

/// [`count_pages`] for a PDF on disk: pdfium reads only the parts of the
/// file it needs. A file the server cannot read is the server's fault and
/// comes back as an untyped error; one it can read but cannot count is a
/// typed [`ConvertFailure`].
pub async fn count_pages_in_file(path: &Path) -> Result<u32> {
    let path: PathBuf = path.to_path_buf();
    bounded(
        &GATE,
        COUNT_QUEUE_TIMEOUT,
        COUNT_WORK_TIMEOUT,
        crate::pdfium::fault_wedged,
        move || {
            use std::io::Read;
            let file = std::fs::File::open(&path).map_err(|e| {
                anyhow::anyhow!("reading {} to count its pages: {e}", path.display())
            })?;
            let len = file
                .metadata()
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", path.display()))?
                .len();
            if len > MAX_PDF_BYTES as u64 {
                return Err(too_big(len));
            }
            let mut head = Vec::with_capacity(HEADER_PROBE_BYTES);
            file.take(HEADER_PROBE_BYTES as u64)
                .read_to_end(&mut head)
                .map_err(|e| {
                    anyhow::anyhow!("reading {} to count its pages: {e}", path.display())
                })?;
            check_header(&head)?;
            with_parser(|parser| parser.count_pages_in_file(&path))
        },
    )
    .await
}

/// Run `work` (which holds pdfium) on the blocking pool behind `gate`, with
/// a wall-clock budget on each of the two waits. See the module docs for
/// what each timeout means to the caller.
async fn bounded<T: Send + 'static>(
    gate: &'static Semaphore,
    queue_wait: Duration,
    work_budget: Duration,
    on_budget_expiry: fn(Duration),
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = match tokio::time::timeout(queue_wait, gate.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(closed)) => anyhow::bail!("the PDF page counter is shut down: {closed}"),
        Err(_) => anyhow::bail!(
            "the PDF page counter was busy for {queue_wait:?}: an earlier document is still \
             inside the PDF parser"
        ),
    };
    let done = Arc::new(AtomicBool::new(false));
    let job_done = Arc::clone(&done);
    let job = tokio::task::spawn_blocking(move || {
        let _held = permit;
        let result = work();
        job_done.store(true, Ordering::SeqCst);
        crate::pdfium::clear_wedged();
        result
    });
    match tokio::time::timeout(work_budget, job).await {
        Ok(Ok(result)) => result,
        // A panic is a fault of this host's code, never a verdict on the
        // document: untyped (retried, never stamped `conversion_failed`).
        Ok(Err(join)) if join.is_panic() => {
            tracing::error!(error = %join, "the PDF page counter PANICKED");
            Err(anyhow::anyhow!("the PDF page counter panicked: {join}"))
        }
        Ok(Err(join)) => Err(anyhow::anyhow!("the page-count task was cancelled: {join}")),
        Err(_) => {
            // The call may finish between the timer and the flag: raise the
            // flag only for a call still running, and take it back if the
            // call finished while it was being raised.
            if !done.load(Ordering::SeqCst) {
                on_budget_expiry(work_budget);
                if done.load(Ordering::SeqCst) {
                    crate::pdfium::clear_wedged();
                }
            }
            Err(ConvertFailure::err(
                FailureCode::PdfParseError,
                format!("the PDF parser did not finish within {work_budget:?}"),
            ))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::classify::{classify, failure_code, FailureClass};
    use std::fmt::Write as _;

    // ── fixtures ────────────────────────────────────────────────────

    /// A PDF assembled by hand: `objs[i]` is object `i + 1`, classic xref.
    /// `prefix` goes between the header line and the first object.
    fn assemble(prefix: &str, objs: &[String]) -> Vec<u8> {
        let mut out = format!("%PDF-1.4\n{prefix}").into_bytes();
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1);
        for off in &offsets {
            writeln!(tail, "{off:010} 00000 n ").unwrap();
        }
        write!(
            tail,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .unwrap();
        out.extend_from_slice(tail.as_bytes());
        out
    }

    /// A well-formed PDF with `pages` empty pages in one flat page tree.
    pub(crate) fn pdf_with_pages(pages: usize) -> Vec<u8> {
        let kids = (0..pages)
            .map(|i| format!("{} 0 R", i + 3))
            .collect::<Vec<_>>()
            .join(" ");
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>"),
        ];
        objs.extend(
            (0..pages)
                .map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string()),
        );
        assemble("", &objs)
    }

    fn nested(depth: usize) -> String {
        format!("{}{}", "[".repeat(depth), "]".repeat(depth))
    }

    /// One page whose dictionary holds an array nested `depth` deep.
    /// `stray_stream` puts a `stream` keyword with no `endstream` before the
    /// objects (the construction that blinded the old nesting pre-scan).
    pub(crate) fn pdf_with_nested_page_entry(depth: usize, stray_stream: bool) -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            // No /Count: pdfium trusts a plausible /Count and never opens the
            // pages, so the tree has to be walked to count it.
            "<< /Type /Pages /Kids [3 0 R] >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /X {} >>",
                nested(depth)
            ),
        ];
        assemble(if stray_stream { "\nstream\n" } else { "" }, &objs)
    }

    /// A zlib stream of stored (uncompressed) deflate blocks: valid input
    /// for a `FlateDecode` filter without a compressor.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks = data.chunks(65_535).peekable();
        while let Some(chunk) = chunks.next() {
            out.push(u8::from(chunks.peek().is_none()));
            out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// The page itself — with an array nested `depth` deep in its
    /// dictionary — lives in a compressed object stream, reached through an
    /// xref stream: bytes no scan of the file's visible syntax can see.
    pub(crate) fn pdf_with_nested_page_in_object_stream(depth: usize) -> Vec<u8> {
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /X {} >>",
            nested(depth)
        );
        let index = "5 0 ";
        let compressed = zlib_stored(format!("{index}{page}").as_bytes());

        let mut out = b"%PDF-1.5\n".to_vec();
        let push = |out: &mut Vec<u8>, bytes: &[u8]| {
            let at = out.len();
            out.extend_from_slice(bytes);
            at
        };
        let off1 = push(
            &mut out,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        let off2 = push(
            &mut out,
            b"2 0 obj\n<< /Type /Pages /Kids [5 0 R] >>\nendobj\n",
        );
        let mut objstm = format!(
            "4 0 obj\n<< /Type /ObjStm /N 1 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
            index.len(),
            compressed.len()
        )
        .into_bytes();
        objstm.extend_from_slice(&compressed);
        objstm.extend_from_slice(b"\nendstream\nendobj\n");
        let off4 = push(&mut out, &objstm);

        // xref stream, W [1 4 2]: type, offset / object-stream number, gen / index.
        let entry = |kind: u8, a: u32, b: u16| {
            let mut e = vec![kind];
            e.extend_from_slice(&a.to_be_bytes());
            e.extend_from_slice(&b.to_be_bytes());
            e
        };
        let off6 = out.len();
        let mut rows = Vec::new();
        rows.extend(entry(0, 0, 65_535));
        rows.extend(entry(1, off1 as u32, 0));
        rows.extend(entry(1, off2 as u32, 0));
        rows.extend(entry(0, 0, 0));
        rows.extend(entry(1, off4 as u32, 0));
        rows.extend(entry(2, 4, 0));
        rows.extend(entry(1, off6 as u32, 0));
        let mut xref = format!(
            "6 0 obj\n<< /Type /XRef /Size 7 /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
            rows.len()
        )
        .into_bytes();
        xref.extend_from_slice(&rows);
        xref.extend_from_slice(b"\nendstream\nendobj\n");
        out.extend_from_slice(&xref);
        out.extend_from_slice(format!("startxref\n{off6}\n%%EOF\n").as_bytes());
        out
    }

    pub(crate) fn pdfium_available() -> bool {
        // Dropped at once: a parser held on this thread would make the
        // counting job (another thread) wait for it forever.
        match crate::pdfium::require() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("libpdfium cannot be bound here: {e:#}");
                false
            }
        }
    }

    /// Return from the test with an explicit SKIPPED line when libpdfium
    /// cannot be bound (CI runners without it).
    macro_rules! skip_without_pdfium {
        ($name:expr) => {
            if !$crate::pdf_meta::tests::pdfium_available() {
                eprintln!("SKIPPED {}: libpdfium cannot be bound here", $name);
                return;
            }
        };
    }
    pub(crate) use skip_without_pdfium;

    // ── child-process harness ───────────────────────────────────────
    //
    // A hostile document is the one input that can end the whole process
    // (the old counter died with SIGABRT on a stack overflow), so these
    // tests count it in a re-executed copy of this test binary and assert on
    // the child's exit status. `child_entry` is that copy's entry point; in
    // a normal run (no environment variable) it does nothing.

    const CHILD_ENV: &str = "HS_SCRIBE_PDF_CHILD_FILES";

    #[test]
    fn child_entry() {
        let Ok(files) = std::env::var(CHILD_ENV) else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for file in files.split('\n') {
            let line = match runtime.block_on(count_pages_in_file(Path::new(file))) {
                Ok(pages) => format!("RESULT ok {pages}"),
                Err(e) => match failure_code(&e) {
                    Some(code) => format!("RESULT refused {}", code.wire()),
                    None => format!("RESULT host-error {e:#}"),
                },
            };
            println!("{line}");
        }
        let peak_kb = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmHWM:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        println!("PEAK_KB {peak_kb}");
        std::process::exit(0);
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Pages(u32),
        Refused(String),
        HostError(String),
    }

    /// Count each of `pdfs` in a child process. Panics (failing the test)
    /// unless the child exits normally. Returns the outcomes and the
    /// child's peak RSS in kB (0 where /proc is unavailable).
    fn count_in_child(dir: &Path, pdfs: &[Vec<u8>]) -> (Vec<Outcome>, u64) {
        let mut paths = Vec::new();
        for (i, bytes) in pdfs.iter().enumerate() {
            let path = dir.join(format!("case-{i}.pdf"));
            std::fs::write(&path, bytes).unwrap();
            paths.push(path.display().to_string());
        }
        let child =
            crate::child_proc::run("pdf_meta::tests::child_entry", CHILD_ENV, &paths.join("\n"));
        let stdout = child.stdout;
        let mut outcomes = Vec::new();
        let mut peak = 0;
        for line in stdout.lines() {
            // libtest prints "test <name> ... " without a newline, so the
            // child's first line shares its line with that prefix.
            let line = line.find("RESULT ").map_or(line, |at| &line[at..]);
            if let Some(rest) = line.strip_prefix("RESULT ok ") {
                outcomes.push(Outcome::Pages(rest.parse().unwrap()));
            } else if let Some(rest) = line.strip_prefix("RESULT refused ") {
                outcomes.push(Outcome::Refused(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix("RESULT host-error ") {
                outcomes.push(Outcome::HostError(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix("PEAK_KB ") {
                peak = rest.parse().unwrap();
            }
        }
        assert_eq!(outcomes.len(), pdfs.len(), "child output: {stdout}");
        (outcomes, peak)
    }

    /// Peak resident memory a child may reach counting any fixture below.
    /// The inputs are at most a few MB; the old counter's neighbours here
    /// allocated gigabytes.
    const CHILD_PEAK_LIMIT_KB: u64 = 512 * 1024;

    fn refused(wire: &str) -> Outcome {
        Outcome::Refused(wire.to_string())
    }

    // ── tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn counts_the_pages_of_a_valid_pdf() {
        skip_without_pdfium!("counts_the_pages_of_a_valid_pdf");
        for pages in [1usize, 7, 120] {
            let n = count_pages(Bytes::from(pdf_with_pages(pages)))
                .await
                .unwrap();
            assert_eq!(n as usize, pages);
        }
        // From disk: same answer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pdf");
        std::fs::write(&path, pdf_with_pages(5)).unwrap();
        assert_eq!(count_pages_in_file(&path).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn html_and_binary_bodies_are_named_for_what_they_are() {
        // Decided by the header gate: no libpdfium needed.
        for (body, code) in [
            (
                &b"<!DOCTYPE html><html><body>please log in</body></html>"[..],
                FailureCode::UnsupportedContentTypeHtml,
            ),
            (
                &[0u8, 1, 2, 3, 4, 5, 6, 7][..],
                FailureCode::UnsupportedContentTypeBinary,
            ),
            (&b""[..], FailureCode::UnsupportedContentTypeBinary),
        ] {
            let err = count_pages(Bytes::copy_from_slice(body)).await.unwrap_err();
            assert_eq!(failure_code(&err), Some(code));
            assert!(matches!(classify(&err), FailureClass::Permanent(_)));
        }
    }

    #[tokio::test]
    async fn a_broken_pdf_is_a_typed_failure_not_an_unknown_page_count() {
        skip_without_pdfium!("a_broken_pdf_is_a_typed_failure_not_an_unknown_page_count");
        for bytes in [
            &b"%PDF-1.4\n"[..],
            b"%PDF-1.4\n%%EOF",
            b"%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\nstartxref\n99999\n%%EOF",
        ] {
            let err = count_pages(Bytes::copy_from_slice(bytes))
                .await
                .unwrap_err();
            assert_eq!(
                failure_code(&err),
                Some(FailureCode::PdfParseError),
                "{err:#}"
            );
        }
    }

    /// A PDF whose xref offsets and `startxref` are wrong — the shape of a
    /// damaged download. The structure is intact; only the index is not.
    fn pdf_with_a_damaged_xref(pages: usize) -> Vec<u8> {
        let mut good = pdf_with_pages(pages);
        let at = good
            .windows(9)
            .rposition(|w| w == b"startxref")
            .expect("fixture has a startxref");
        good.truncate(at);
        good.extend_from_slice(b"startxref\n12345\n%%EOF\n");
        good
    }

    #[tokio::test]
    async fn a_pdf_the_renderer_repairs_is_counted_not_refused() {
        // The old lopdf counter refused this with "Invalid cross-reference
        // table" and the watcher stamped it `pdf_parse_error` for good —
        // while pdfium, which renders it, reads it fine (review F8).
        skip_without_pdfium!("a_pdf_the_renderer_repairs_is_counted_not_refused");
        let n = count_pages(Bytes::from(pdf_with_a_damaged_xref(3)))
            .await
            .unwrap();
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn a_pdf_with_no_pages_is_refused() {
        skip_without_pdfium!("a_pdf_with_no_pages_is_refused");
        let err = count_pages(Bytes::from(pdf_with_pages(0)))
            .await
            .unwrap_err();
        assert_eq!(
            failure_code(&err),
            Some(FailureCode::PdfParseError),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_a_host_error_not_a_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let err = count_pages_in_file(&dir.path().join("gone.pdf"))
            .await
            .unwrap_err();
        assert_eq!(failure_code(&err), None, "{err:#}");
        assert_eq!(classify(&err), FailureClass::Transient);
    }

    #[test]
    fn hostile_nesting_cannot_end_the_process() {
        skip_without_pdfium!("hostile_nesting_cannot_end_the_process");
        let dir = tempfile::tempdir().unwrap();
        // Each of these made the previous counter abort the process with a
        // stack overflow (verified: exit status 134). The first is the
        // review's construction: a stray `stream` keyword hides the nesting
        // from any syntax pre-scan; the second needs no trick at all; the
        // third hides the page in a compressed object stream.
        let pdfs = vec![
            pdf_with_nested_page_entry(100_000, true),
            pdf_with_nested_page_entry(100_000, false),
            pdf_with_nested_page_entry(1_000_000, false),
            pdf_with_nested_page_in_object_stream(100_000),
            pdf_with_nested_page_in_object_stream(1_000_000),
        ];
        let (outcomes, peak_kb) = count_in_child(dir.path(), &pdfs);
        eprintln!("hostile fixtures: {outcomes:?}, child peak RSS {peak_kb} kB");
        for (i, outcome) in outcomes.iter().enumerate() {
            // An answer or a typed refusal — never a host error, never an abort.
            assert!(
                matches!(outcome, Outcome::Pages(1) | Outcome::Refused(_)),
                "case {i}: {outcome:?}"
            );
        }
        assert!(
            peak_kb < CHILD_PEAK_LIMIT_KB,
            "the child reached {peak_kb} kB resident"
        );
    }

    #[test]
    fn ordinary_nesting_still_counts_and_the_fixtures_are_well_formed() {
        skip_without_pdfium!("ordinary_nesting_still_counts_and_the_fixtures_are_well_formed");
        let dir = tempfile::tempdir().unwrap();
        // Shallow versions of the hostile fixtures are real, countable PDFs:
        // the hostile cases above fail (or pass) because of their depth, not
        // because the builder emits garbage. The object-stream case proves
        // pdfium reaches the compressed page (count 1 needs it).
        let pdfs = vec![
            pdf_with_nested_page_entry(10, false),
            pdf_with_nested_page_entry(10, true),
            pdf_with_nested_page_in_object_stream(10),
            pdf_with_pages(3),
        ];
        let (outcomes, _) = count_in_child(dir.path(), &pdfs);
        assert_eq!(
            outcomes,
            vec![
                Outcome::Pages(1),
                Outcome::Pages(1),
                Outcome::Pages(1),
                Outcome::Pages(3)
            ]
        );
    }

    #[test]
    fn a_document_with_more_pages_than_can_be_indexed_is_refused_not_truncated() {
        skip_without_pdfium!("a_document_with_more_pages_than_can_be_indexed");
        let dir = tempfile::tempdir().unwrap();
        // 65 600 real pages: pdfium-render's `len()` would say 64.
        let (outcomes, _) = count_in_child(dir.path(), &[pdf_with_pages(65_600)]);
        assert_eq!(outcomes, vec![refused("pdf_parse_error")]);
    }

    #[test]
    fn corrupting_or_truncating_a_valid_pdf_never_ends_the_process() {
        skip_without_pdfium!("corrupting_or_truncating_a_valid_pdf_never_ends_the_process");
        let dir = tempfile::tempdir().unwrap();
        let good = pdf_with_pages(4);
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for cut in (0..good.len()).step_by(11) {
            cases.push(good[..cut].to_vec());
        }
        for i in (8..good.len()).step_by(13) {
            let mut bad = good.clone();
            bad[i] ^= 0xFF;
            cases.push(bad);
        }
        let (outcomes, _) = count_in_child(dir.path(), &cases);
        for (i, outcome) in outcomes.iter().enumerate() {
            match outcome {
                // A prefix can still be a whole PDF when only trailing
                // bytes are cut, and pdfium repairs some damage.
                Outcome::Pages(n) => assert!((1..=4).contains(n), "case {i}: {n}"),
                Outcome::Refused(_) => {}
                Outcome::HostError(e) => panic!("case {i} was a host error: {e}"),
            }
        }
    }

    #[tokio::test]
    async fn a_stuck_parse_is_refused_at_its_deadline_and_the_queue_behind_it_is_told_to_retry() {
        static LOCAL_GATE: Semaphore = Semaphore::const_new(1);
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let stuck = tokio::spawn(bounded(
            &LOCAL_GATE,
            Duration::from_secs(5),
            Duration::from_millis(100),
            |_| {},
            move || {
                // Stands in for a pdfium call that does not return.
                let _ = hold.recv_timeout(Duration::from_secs(30));
                Ok(1u32)
            },
        ));
        let err = stuck.await.unwrap().unwrap_err();
        assert_eq!(
            failure_code(&err),
            Some(FailureCode::PdfParseError),
            "{err:#}"
        );

        // The abandoned parse still holds the gate: the next caller waits
        // for its turn, gives up, and is told to retry — it gets no verdict
        // on its own (perfectly good) document.
        let err = bounded(
            &LOCAL_GATE,
            Duration::from_millis(100),
            Duration::from_secs(5),
            |_| {},
            || Ok(2u32),
        )
        .await
        .unwrap_err();
        assert_eq!(failure_code(&err), None, "{err:#}");
        assert_eq!(classify(&err), FailureClass::Transient);

        // Once pdfium returns, counting works again.
        release.send(()).unwrap();
        let ok = bounded(
            &LOCAL_GATE,
            Duration::from_secs(5),
            Duration::from_secs(5),
            |_| {},
            || Ok(3u32),
        )
        .await
        .unwrap();
        assert_eq!(ok, 3);
    }

    #[tokio::test]
    async fn a_panic_is_never_a_verdict_on_the_document() {
        static LOCAL_GATE: Semaphore = Semaphore::const_new(1);
        let err = bounded(
            &LOCAL_GATE,
            Duration::from_secs(5),
            Duration::from_secs(5),
            |_| {},
            || -> Result<u32> { panic!("index out of bounds in xref") },
        )
        .await
        .unwrap_err();
        // Untyped: the watcher retries it and never stamps conversion_failed.
        assert_eq!(failure_code(&err), None, "{err:#}");
        assert_eq!(classify(&err), FailureClass::Transient);
        // The gate is released by the unwinding thread.
        assert_eq!(
            bounded(
                &LOCAL_GATE,
                Duration::from_secs(5),
                Duration::from_secs(5),
                |_| {},
                || Ok(1u32)
            )
            .await
            .unwrap(),
            1
        );
    }

    // ── pdfium faults, in a child process (they end the process) ────

    const FAULT_ENV: &str = "HS_SCRIBE_PDF_FAULT_CHILD";

    /// Child entry. `panic-with-parser`: a panic while a parser is alive
    /// (through `with_parser`) must leave pdfium usable. `poison`: the raw
    /// hazard — a panic unwinding through a live `PdfParser` — must turn
    /// into red health and a process exit. `wedge`: a call past its budget
    /// must do the same. `recover`: a call that returns within the grace
    /// period must not.
    #[test]
    fn fault_child_entry() {
        let Ok(mode) = std::env::var(FAULT_ENV) else {
            return;
        };
        crate::pdfium::install_fault_exit(Duration::from_millis(700));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let pdf = Bytes::from(pdf_with_pages(1));
        rt.block_on(async {
            let one =
                |pdf: Bytes| async move { count_pages(pdf).await.map_err(|e| format!("{e:#}")) };
            println!("RESULT before {:?}", one(pdf.clone()).await);
            match mode.as_str() {
                "panic-with-parser" => {
                    let joined = tokio::task::spawn_blocking(|| {
                        crate::pdfium::with_parser(|_| -> Result<()> {
                            panic!("host code panicked")
                        })
                    })
                    .await;
                    println!("RESULT panicked {}", joined.unwrap_err().is_panic());
                    println!("RESULT after {:?}", one(pdf.clone()).await);
                    println!("RESULT healthy {:?}", crate::pdfium::healthy());
                }
                "poison" => {
                    let joined = std::thread::spawn(|| {
                        let _parser = crate::pdfium::PdfParser::new().unwrap();
                        panic!("host code panicked with a live parser");
                    })
                    .join();
                    println!("RESULT panicked {}", joined.is_err());
                    let after = count_pages(pdf.clone()).await;
                    println!(
                        "RESULT after typed={:?}",
                        after.as_ref().err().and_then(failure_code)
                    );
                    println!("RESULT healthy {:?}", crate::pdfium::healthy().is_ok());
                }
                "wedge" | "recover" => {
                    static GATE: Semaphore = Semaphore::const_new(1);
                    let hold = if mode == "wedge" { 30_000 } else { 300 };
                    let r = bounded(
                        &GATE,
                        Duration::from_secs(5),
                        Duration::from_millis(100),
                        crate::pdfium::fault_wedged,
                        move || {
                            std::thread::sleep(Duration::from_millis(hold));
                            Ok(1u32)
                        },
                    )
                    .await;
                    println!(
                        "RESULT refused {:?}",
                        r.err().and_then(|e| failure_code(&e))
                    );
                    println!("RESULT healthy-now {:?}", crate::pdfium::healthy().is_ok());
                }
                "render-wedge" | "render-slow" => {
                    // The Legacy render path: one pdfium call under the watchdog.
                    crate::pdfium::set_call_budget(Duration::from_millis(200));
                    let hold = if mode == "render-wedge" { 30_000 } else { 600 };
                    tokio::task::spawn_blocking(move || {
                        crate::pdfium::guarded_call(|| {
                            std::thread::sleep(Duration::from_millis(hold))
                        })
                    })
                    .await
                    .unwrap();
                    println!("RESULT healthy-now {:?}", crate::pdfium::healthy().is_ok());
                }
                other => panic!("unknown mode {other}"),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            println!("RESULT survived-grace {:?}", crate::pdfium::healthy());
        });
        std::process::exit(0);
    }

    fn fault_child(mode: &str) -> (Option<i32>, Vec<String>) {
        let (code, stdout) = crate::child_proc::run_expecting_exit(
            "pdf_meta::tests::fault_child_entry",
            FAULT_ENV,
            mode,
        );
        let lines = stdout
            .lines()
            .filter_map(|l| l.find("RESULT ").map(|at| l[at + 7..].to_string()))
            .collect();
        (code, lines)
    }

    #[test]
    fn a_panic_while_a_parser_is_alive_does_not_poison_pdfium() {
        skip_without_pdfium!("a_panic_while_a_parser_is_alive_does_not_poison_pdfium");
        let (code, lines) = fault_child("panic-with-parser");
        assert_eq!(code, Some(0), "{lines:?}");
        assert_eq!(lines[0], "before Ok(1)");
        assert_eq!(lines[1], "panicked true");
        // Before the fix this was an Err on every call, for ever.
        assert_eq!(lines[2], "after Ok(1)", "{lines:?}");
        assert_eq!(lines[3], "healthy Ok(())");
    }

    #[test]
    fn a_poisoned_pdfium_lock_turns_health_red_and_exits_non_zero() {
        skip_without_pdfium!("a_poisoned_pdfium_lock_turns_health_red_and_exits_non_zero");
        let (code, lines) = fault_child("poison");
        assert_eq!(lines[0], "before Ok(1)");
        assert_eq!(lines[1], "panicked true");
        // Never a verdict about the document...
        assert_eq!(lines[2], "after typed=None", "{lines:?}");
        // ...health is red, and the process exits before the grace sleep ends.
        assert_eq!(lines[3], "healthy false");
        assert_eq!(code, Some(crate::pdfium::FAULT_EXIT_CODE), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("survived-grace")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_wedged_pdfium_call_turns_health_red_and_exits_non_zero() {
        let (code, lines) = fault_child("wedge");
        assert_eq!(code, Some(crate::pdfium::FAULT_EXIT_CODE), "{lines:?}");
        assert!(lines.iter().any(|l| l == "healthy-now false"), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("survived-grace")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_wedged_render_call_turns_health_red_and_exits_non_zero() {
        let (code, lines) = fault_child("render-wedge");
        assert_eq!(code, Some(crate::pdfium::FAULT_EXIT_CODE), "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.starts_with("survived-grace")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_slow_render_call_that_returns_within_the_grace_period_does_not_end_the_process() {
        let (code, lines) = fault_child("render-slow");
        assert_eq!(code, Some(0), "{lines:?}");
        assert!(lines.iter().any(|l| l == "healthy-now true"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "survived-grace Ok(())"),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn a_call_that_finishes_exactly_at_its_budget_never_leaves_the_flag_raised() {
        // The race: the timer fires, the call returns, then the flag is set.
        // Finishing in the same instant as the budget, many times, must leave
        // pdfium healthy (a stale flag would exit the process for nothing).
        static LOCAL_GATE: Semaphore = Semaphore::const_new(1);
        for _ in 0..40 {
            let _ = bounded(
                &LOCAL_GATE,
                Duration::from_secs(5),
                Duration::from_millis(5),
                crate::pdfium::fault_wedged,
                || {
                    std::thread::sleep(Duration::from_millis(5));
                    Ok(1u32)
                },
            )
            .await;
            // Let the job's own clear run, then check.
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(crate::pdfium::healthy().is_ok(), "stale wedge flag");
        }
    }

    #[test]
    fn a_slow_pdfium_call_that_returns_within_the_grace_period_does_not_end_the_process() {
        let (code, lines) = fault_child("recover");
        assert_eq!(code, Some(0), "{lines:?}");
        assert!(lines.iter().any(|l| l == "healthy-now false"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "survived-grace Ok(())"),
            "{lines:?}"
        );
    }
}
