//! Whole-PDF converter that shells out to the `olmocr` CLI.
//!
//! Olmocr (allenai/olmOCR-2-7B-1025-FP8 via vLLM) handles render +
//! anchor + prompt + parse + assemble end-to-end. The scribe server
//! exposes it as a peer of the legacy per-region pipeline; the
//! `cmd_watch_events` chain in `crates/hs/src/scribe_cmd.rs` selects
//! between them by URL.
//!
//! Wire shape:
//!
//! ```text
//! POST /scribe/stream (PDF bytes) → upload streamed to /tmp/<random>.pdf
//!     → convert(path, source_pages, ...)
//!     → mkdir workspace/{results,worker_locks,done_flags}
//!     → olmocr workspace --server <endpoint> --model <model>
//!                        --pdfs <pdf>
//!                        --workers 1 --max_concurrent_requests 4
//!     → read workspace/results/output_<hash>.jsonl (one Dolma document)
//!     → cut its text at `attributes.pdf_page_numbers` and join the pages
//!       with the page separator
//! ```
//!
//! Concurrency cap rationale: olmocr's default `--max_concurrent_requests`
//! is high enough to overrun vLLM's `--max-num-seqs`, which crashes the
//! APIServer (observed during the manual Phase A backfill). Capping at
//! 4 matches the vLLM serve config on big and stays stable indefinitely.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::classify::{ConvertFailure, FailureCode};
use crate::config::AppConfig;

/// Convert the PDF at `pdf_path` (`source_pages` pages, counted from the
/// file by the caller) to markdown via the `olmocr` CLI.
///
/// The markdown is returned only when olmocr accounts for every page it was
/// given: `Completed pages` equal to `source_pages` and `Failed pages` of
/// zero. A run that completed some pages and failed others leaves a
/// markdown file with holes in it, and handing that back as a success
/// stamps a partial book as converted. Those runs fail with an
/// `Escalate`-class [`ConvertFailure`] so the next backend gets the document.
pub async fn convert(pdf_path: &Path, source_pages: u32, config: &AppConfig) -> Result<String> {
    let workspace = tempfile::Builder::new()
        .prefix("hs-scribe-olmocr-")
        .tempdir()
        .context("creating olmocr workspace tempdir")?;

    let workspace_path = workspace.path().to_path_buf();

    tracing::info!(
        endpoint = %config.olmocr_endpoint,
        model = %config.olmocr_model,
        workspace = %workspace_path.display(),
        source_pages,
        "olmocr_subprocess: invoking CLI"
    );

    let output = tokio::process::Command::new(&config.olmocr_bin)
        .arg(&workspace_path)
        .args([
            "--server",
            &config.olmocr_endpoint,
            "--model",
            &config.olmocr_model,
        ])
        .arg("--pdfs")
        .arg(pdf_path)
        .args(["--workers", "1", "--max_concurrent_requests", "4"])
        // If the handler future is dropped (client disconnect, convert
        // deadline), kill the CLI rather than orphan a process that keeps
        // hammering vLLM for the rest of a 45-minute book.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawning olmocr CLI at `{}`", config.olmocr_bin))?;

    if !output.status.success() {
        // The message becomes the wire `Error` line and the log line, so it
        // carries the end of the CLI's account, not all of a long run's.
        return Err(anyhow!(
            "olmocr CLI exited with status {:?}: {}",
            output.status.code(),
            stderr_tail(&String::from_utf8_lossy(&output.stderr))
        ));
    }

    // olmocr emits its end-of-run "Completed pages: N" / "Failed pages: N"
    // summary through Python logging: pipeline.py calls `logger.info(...)` on a
    // bare `logging.StreamHandler()`, which defaults to STDERR — not stdout.
    // Parse BOTH streams so the count is found wherever olmocr writes it.
    // Reading only stdout made `completed` parse as 0 on every successful run,
    // so good olmocr output — notably the code-dense books olmocr exists to
    // handle — was silently discarded and escalated to the next backend.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (completed, failed) = parse_page_counts(&format!("{stdout}\n{stderr}"));
    tracing::info!(
        completed,
        failed,
        source_pages,
        "olmocr_subprocess: CLI exited cleanly"
    );
    if let Err(failure) = check_page_accounting(completed, failed, source_pages) {
        // The workspace tempdir is about to be dropped; keep the CLI's own
        // account of what went wrong for the post-mortem.
        let tail = stderr_tail(&stderr);
        tracing::warn!(
            completed,
            failed,
            source_pages,
            stderr_tail = %tail,
            "olmocr_subprocess: {failure} — escalating to next backend"
        );
        return Err(anyhow::Error::new(failure));
    }

    let doc = read_result_document(&workspace_path)?;
    paginate(&doc, source_pages)
}

/// The part of olmocr's Dolma result document the converter uses: the
/// text and where each PDF page sits in it.
#[derive(Debug, serde::Deserialize)]
struct ResultDocument {
    text: String,
    attributes: ResultAttributes,
}

#[derive(Debug, serde::Deserialize)]
struct ResultAttributes {
    /// `[start, end, page]` per page, in page order. `start`/`end` index
    /// the text in **characters** (Python string positions), not bytes.
    pdf_page_numbers: Vec<(usize, usize, u32)>,
}

/// Read the one result document olmocr wrote for the one PDF it was given.
///
/// olmocr writes `results/output_<hash>.jsonl`, one JSON document per
/// line. Its `--markdown` option would also write the same text flattened
/// to a `.md` file, which loses the page boundaries; that is why the
/// converter reads the result document and not the markdown file.
fn read_result_document(workspace: &Path) -> Result<ResultDocument> {
    let results = workspace.join("results");
    let mut lines: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&results).with_context(|| {
        format!(
            "olmocr wrote no results directory at `{}`",
            results.display()
        )
    })? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading olmocr result `{}`", path.display()))?;
        lines.extend(
            body.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned),
        );
    }
    let [line] = lines.as_slice() else {
        return Err(anyhow!(
            "olmocr wrote {} result documents under `{}` for one PDF; expected exactly 1",
            lines.len(),
            results.display()
        ));
    };
    serde_json::from_str(line).with_context(|| {
        format!(
            "olmocr's result document under `{}` is not the expected Dolma shape \
             (text + attributes.pdf_page_numbers)",
            results.display()
        )
    })
}

/// Cut the result document's text into its PDF pages and join them with
/// the page separator, so the page boundaries olmocr recorded survive into
/// the markdown (and from there into the catalog's page offsets and every
/// search hit's `page`).
///
/// The spans must account for exactly the `source_pages` pages, in order,
/// tiling the whole text. Anything else means olmocr's output does not
/// describe the PDF it was given, and is refused like an incomplete run.
fn paginate(doc: &ResultDocument, source_pages: u32) -> Result<String> {
    use crate::pipeline::markdown_generator::{join_pages, PAGE_SEPARATOR};

    let spans = &doc.attributes.pdf_page_numbers;
    let incomplete = |detail: String| {
        anyhow::Error::new(ConvertFailure::new(
            FailureCode::OlmocrIncompletePages,
            format!("olmocr's page spans do not describe the {source_pages}-page PDF: {detail}"),
        ))
    };
    if spans.len() != source_pages as usize {
        return Err(incomplete(format!("{} spans", spans.len())));
    }

    let text = doc.text.as_str();
    // The byte offset of every character, then the end of the text. Spans
    // are character positions in increasing order, so one pass converts
    // them all.
    let mut boundaries = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain([text.len()]);
    let mut next_char = 0usize;
    let mut byte_of = |target: usize| -> Option<usize> {
        let byte = boundaries.nth(target.checked_sub(next_char)?)?;
        next_char = target + 1;
        Some(byte)
    };

    let mut pages = Vec::with_capacity(spans.len());
    let mut start_byte = 0usize;
    let mut expected_start = 0usize;
    for (i, &(start, end, page)) in spans.iter().enumerate() {
        if page as usize != i + 1 {
            return Err(incomplete(format!(
                "span {i} is page {page}, expected {}",
                i + 1
            )));
        }
        if start != expected_start || end < start {
            return Err(incomplete(format!(
                "page {page} spans characters {start}..{end}, expected to start at {expected_start}"
            )));
        }
        let end_byte = if end == start {
            start_byte
        } else {
            byte_of(end).ok_or_else(|| {
                incomplete(format!(
                    "page {page} ends at character {end}, past the text"
                ))
            })?
        };
        // olmocr ends every page but the last with one "\n" of its own;
        // leading and trailing whitespace of a page carries no content.
        let mut page_text = text[start_byte..end_byte].trim().to_owned();
        // A thematic break as the page's last line would fuse with the
        // separator after it into a second separator; write it as the
        // equivalent `***`, as `join_pages` does inside a page.
        if let Some(body) = page_text.strip_suffix("\n\n---") {
            page_text = format!("{body}\n\n***");
        }
        pages.push(page_text);
        start_byte = end_byte;
        expected_start = end;
    }
    if start_byte != text.len() {
        return Err(incomplete(format!(
            "the spans end at character {expected_start}, before the end of the text"
        )));
    }
    if pages.iter().all(|p| p.is_empty()) {
        return Err(anyhow!("olmocr's result document has no text on any page"));
    }

    let markdown = join_pages(&pages);
    // The invariant every downstream reader depends on: splitting on the
    // separator gives back exactly these pages.
    let found = markdown.split(PAGE_SEPARATOR).count();
    if found != pages.len() {
        return Err(anyhow!(
            "joined {} olmocr pages but the separator splits the markdown into {found}",
            pages.len()
        ));
    }
    Ok(markdown)
}

/// Most lines, and most bytes, of the CLI's stderr put in an error message or
/// log line. A run over a long book logs per page; the failure is at the end.
const STDERR_TAIL_LINES: usize = 15;
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// The last [`STDERR_TAIL_LINES`] lines of `stderr`, cut to at most
/// [`STDERR_TAIL_BYTES`] bytes (from the front, on a character boundary).
fn stderr_tail(stderr: &str) -> String {
    let mut lines: Vec<&str> = stderr.lines().rev().take(STDERR_TAIL_LINES).collect();
    lines.reverse();
    let tail = lines.join("\n");
    if tail.len() <= STDERR_TAIL_BYTES {
        return tail;
    }
    let mut start = tail.len() - STDERR_TAIL_BYTES;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    tail[start..].to_string()
}

/// Decide whether olmocr's end-of-run tally describes a complete
/// conversion of a `source_pages`-page PDF.
///
/// - No completed page at all: olmocr genuinely produced nothing —
///   pypdfium2 couldn't open the PDF, or its output validation rejected
///   every page. That is an olmocr-class failure, not proof the PDF is
///   broken, so the next backend gets its shot (a genuinely broken PDF dies
///   in seconds there with a real parse error).
/// - Any failed page, or a completed count that is not the page count: the
///   markdown olmocr wrote has holes.
fn check_page_accounting(
    completed: u64,
    failed: u64,
    source_pages: u32,
) -> std::result::Result<(), ConvertFailure> {
    if completed == 0 {
        return Err(ConvertFailure::new(
            FailureCode::OlmocrZeroPages,
            format!(
                "olmocr reported 0 completed pages (failed={failed}); content may need a different backend"
            ),
        ));
    }
    if failed > 0 || completed != u64::from(source_pages) {
        return Err(ConvertFailure::new(
            FailureCode::OlmocrIncompletePages,
            format!(
                "olmocr completed {completed} of {source_pages} pages ({failed} failed); \
                 the markdown would have holes"
            ),
        ));
    }
    Ok(())
}

/// Parse the `Completed pages: N` / `Failed pages: N` lines olmocr logs to
/// stderr at the end of a run (the caller passes stdout+stderr combined).
/// Returns `(0, 0)` if the markers aren't found, which `check_page_accounting`
/// reads as "nothing completed".
fn parse_page_counts(stdout: &str) -> (u64, u64) {
    let mut completed = 0u64;
    let mut failed = 0u64;
    for line in stdout.lines() {
        if let Some(rest) = line.split_once("Completed pages:") {
            if let Some(n) = rest.1.split_whitespace().next() {
                completed = n.parse().unwrap_or(0);
            }
        } else if let Some(rest) = line.split_once("Failed pages:") {
            if let Some(n) = rest.1.split_whitespace().next() {
                failed = n.parse().unwrap_or(0);
            }
        }
    }
    (completed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{classify, failure_code, FailureClass};

    #[test]
    fn parse_page_counts_extracts_completed_and_failed() {
        let stdout = "
2026-06-09 11:00:00 - olmocr.pipeline - INFO - Completed pages: 229
2026-06-09 11:00:00 - olmocr.pipeline - INFO - Failed pages: 0
";
        let (c, f) = parse_page_counts(stdout);
        assert_eq!(c, 229);
        assert_eq!(f, 0);
    }

    #[test]
    fn parse_page_counts_handles_missing_markers() {
        let (c, f) = parse_page_counts("no relevant content here");
        assert_eq!(c, 0);
        assert_eq!(f, 0);
    }

    #[test]
    fn parse_page_counts_treats_zero_completed_as_failure_signal() {
        // gamma/brooks shape — olmocr's pypdfium2 fails to open the PDF
        // and exits cleanly with all-zeros stats.
        let stdout = "Completed pages: 0\nFailed pages: 0\n";
        let (c, f) = parse_page_counts(stdout);
        assert_eq!(c, 0);
        assert_eq!(f, 0);
    }

    #[test]
    fn every_page_completed_and_none_failed_is_a_complete_conversion() {
        check_page_accounting(229, 0, 229).unwrap();
        check_page_accounting(1, 0, 1).unwrap();
    }

    #[test]
    fn zero_completed_pages_escalates_as_the_olmocr_zero_page_failure() {
        for failed in [0, 5] {
            let e = check_page_accounting(0, failed, 5).unwrap_err();
            assert_eq!(e.code(), FailureCode::OlmocrZeroPages);
        }
    }

    #[test]
    fn a_failed_page_is_not_a_success_even_when_others_completed() {
        let e = check_page_accounting(228, 1, 229).unwrap_err();
        assert_eq!(e.code(), FailureCode::OlmocrIncompletePages);
        // Failed pages alone, with the completed count looking right.
        let e = check_page_accounting(229, 1, 229).unwrap_err();
        assert_eq!(e.code(), FailureCode::OlmocrIncompletePages);
    }

    #[test]
    fn a_completed_count_that_is_not_the_page_count_is_not_a_success() {
        for completed in [1, 5, 9, 11, 1000] {
            let e = check_page_accounting(completed, 0, 10).unwrap_err();
            assert_eq!(e.code(), FailureCode::OlmocrIncompletePages, "{completed}");
        }
    }

    #[test]
    fn incomplete_runs_escalate_to_the_next_backend_rather_than_stamp_success() {
        let e = anyhow::Error::new(check_page_accounting(3, 1, 4).unwrap_err());
        assert_eq!(
            classify(&e),
            FailureClass::Escalate("olmocr_incomplete_pages")
        );
        let e = anyhow::Error::new(check_page_accounting(0, 0, 4).unwrap_err());
        assert_eq!(classify(&e), FailureClass::Escalate("olmocr_zero_pages"));
    }

    // ── end to end against a stand-in `olmocr` executable ──────────────
    //
    // The real CLI is never run: these tests point `olmocr_bin` at a shell
    // script that writes a result document and prints the tally olmocr prints.

    use super::test_support::result_document;
    #[cfg(unix)]
    use super::test_support::{completed_in, install_stand_in, pages, write_result};
    use crate::pipeline::markdown_generator::PAGE_SEPARATOR;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    fn fake_olmocr(dir: &Path, tally: &str) -> PathBuf {
        let script = dir.join("fake-olmocr.sh");
        let page_texts = pages(completed_in(tally), "body text");
        let refs: Vec<&str> = page_texts.iter().map(String::as_str).collect();
        let body = format!("{}\n{tally}", write_result(&result_document(&refs)));
        install_stand_in(&script, &body);
        script
    }

    #[cfg(unix)]
    async fn run_fake(tally: &str, source_pages: u32) -> Result<String> {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_olmocr(dir.path(), tally);
        let pdf = dir.path().join("in.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        let config = AppConfig {
            olmocr_bin: script.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        convert(&pdf, source_pages, &config).await
    }

    /// The markdown keeps the PDF's pages: one separator-delimited page per
    /// PDF page, which is what the catalog's page offsets — and every search
    /// hit's `page` — are computed from. Before, olmocr's flat text gave one
    /// "page" for the whole document and the offsets were thrown away.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_complete_run_returns_one_markdown_page_per_pdf_page() {
        let md = run_fake(
            "echo 'INFO - Completed pages: 4' >&2; echo 'INFO - Failed pages: 0' >&2",
            4,
        )
        .await
        .unwrap();
        let pieces: Vec<&str> = md.split(PAGE_SEPARATOR).collect();
        assert_eq!(pieces.len(), 4, "{md}");
        for (i, piece) in pieces.iter().enumerate() {
            assert!(piece.starts_with(&format!("# Page {}", i + 1)), "{piece}");
        }
        let offsets = hs_common::catalog::compute_page_offsets(&md);
        let accounting = hs_common::catalog::resolve_page_accounting(offsets.len() as u64, Some(4));
        assert_eq!(accounting.total_pages, 4);
        assert!(accounting.offsets_trustworthy);
    }

    fn doc(text: &str, spans: &[(usize, usize, u32)]) -> ResultDocument {
        ResultDocument {
            text: text.to_owned(),
            attributes: ResultAttributes {
                pdf_page_numbers: spans.to_vec(),
            },
        }
    }

    fn from_pages(page_texts: &[&str]) -> ResultDocument {
        serde_json::from_str(&result_document(page_texts)).unwrap()
    }

    /// olmocr's spans count characters (Python string positions). Read as
    /// byte offsets they cut inside a multi-byte character or put every
    /// later page boundary in the wrong place.
    #[test]
    fn spans_are_character_positions_not_byte_offsets() {
        let page_texts = [
            "Café au lait — naïve",
            "Über die Grenzen 日本語",
            "plain ascii",
        ];
        let md = paginate(&from_pages(&page_texts), 3).unwrap();
        let pieces: Vec<&str> = md.split(PAGE_SEPARATOR).collect();
        assert_eq!(pieces, page_texts);
    }

    /// A page olmocr read no text from (`natural_text: null`) is still a
    /// page: dropping it would shift every later page number by one.
    #[test]
    fn an_empty_page_keeps_its_place() {
        let md = paginate(&from_pages(&["first", "", "third"]), 3).unwrap();
        let pieces: Vec<&str> = md.split(PAGE_SEPARATOR).collect();
        assert_eq!(pieces, ["first", "", "third"]);
    }

    /// A thematic break as a page's last line would fuse with the separator
    /// that follows it and split the page in two.
    #[test]
    fn a_rule_at_the_end_of_a_page_does_not_add_a_page() {
        let md = paginate(&from_pages(&["text\n\n---", "next", "last"]), 3).unwrap();
        let pieces: Vec<&str> = md.split(PAGE_SEPARATOR).collect();
        assert_eq!(pieces, ["text\n\n***", "next", "last"]);
    }

    #[test]
    fn spans_for_a_different_page_count_are_refused_as_incomplete() {
        let e = paginate(&from_pages(&["a", "b"]), 3).unwrap_err();
        assert_eq!(failure_code(&e), Some(FailureCode::OlmocrIncompletePages));
    }

    #[test]
    fn spans_that_do_not_tile_the_text_are_refused_as_incomplete() {
        for (what, spans) in [
            ("a gap", vec![(0, 2, 1), (3, 5, 2)]),
            ("out of order", vec![(0, 2, 2), (2, 5, 1)]),
            ("past the text", vec![(0, 2, 1), (2, 9, 2)]),
            ("short of the text", vec![(0, 2, 1), (2, 4, 2)]),
        ] {
            let e = paginate(&doc("ab\ncd", &spans), 2).unwrap_err();
            assert_eq!(
                failure_code(&e),
                Some(FailureCode::OlmocrIncompletePages),
                "{what}: {e:#}"
            );
        }
    }

    #[test]
    fn a_document_with_no_text_on_any_page_is_refused() {
        let e = paginate(&from_pages(&["", ""]), 2).unwrap_err();
        assert!(format!("{e:#}").contains("no text"), "{e:#}");
    }

    #[test]
    fn a_missing_or_ambiguous_result_is_an_error_naming_the_results_directory() {
        let dir = tempfile::tempdir().unwrap();
        let e = read_result_document(dir.path()).unwrap_err();
        assert!(format!("{e:#}").contains("results"), "{e:#}");

        let results = dir.path().join("results");
        std::fs::create_dir(&results).unwrap();
        let one = result_document(&["a"]);
        std::fs::write(results.join("output_x.jsonl"), format!("{one}\n{one}\n")).unwrap();
        let e = read_result_document(dir.path()).unwrap_err();
        assert!(format!("{e:#}").contains("2 result documents"), "{e:#}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_with_a_failed_page_is_an_escalating_failure_not_partial_markdown() {
        let err = run_fake(
            "echo 'INFO - Completed pages: 3' >&2; echo 'INFO - Failed pages: 1' >&2",
            4,
        )
        .await
        .unwrap_err();
        assert_eq!(
            failure_code(&err),
            Some(FailureCode::OlmocrIncompletePages),
            "{err:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_that_completed_fewer_pages_than_the_source_has_is_refused() {
        let err = run_fake(
            "echo 'INFO - Completed pages: 2' >&2; echo 'INFO - Failed pages: 0' >&2",
            4,
        )
        .await
        .unwrap_err();
        assert_eq!(
            failure_code(&err),
            Some(FailureCode::OlmocrIncompletePages),
            "{err:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_with_no_tally_at_all_counts_as_zero_completed_pages() {
        let err = run_fake("echo 'something unrelated' >&2", 4)
            .await
            .unwrap_err();
        assert_eq!(
            failure_code(&err),
            Some(FailureCode::OlmocrZeroPages),
            "{err:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_nonzero_exit_is_an_untyped_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("boom.sh");
        install_stand_in(&script, "echo 'vllm unreachable' >&2\nexit 3");
        let pdf = dir.path().join("in.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        let config = AppConfig {
            olmocr_bin: script.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let err = convert(&pdf, 1, &config).await.unwrap_err();
        assert_eq!(failure_code(&err), None);
        assert!(format!("{err:#}").contains("vllm unreachable"), "{err:#}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_run_reports_the_end_of_its_stderr_not_all_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("loud.sh");
        install_stand_in(
            &script,
            "echo FIRST-LINE-MARKER >&2\n\
             i=0; while [ $i -lt 5000 ]; do echo \"page $i done\" >&2; i=$((i+1)); done\n\
             echo 'fatal: backend went away' >&2\nexit 3",
        );
        let pdf = dir.path().join("in.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        let config = AppConfig {
            olmocr_bin: script.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        let message = format!("{:#}", convert(&pdf, 1, &config).await.unwrap_err());
        assert!(message.contains("fatal: backend went away"), "{message}");
        assert!(!message.contains("FIRST-LINE-MARKER"), "{message}");
        assert!(
            message.len() < STDERR_TAIL_BYTES + 200,
            "{} bytes",
            message.len()
        );
    }

    #[test]
    fn the_stderr_tail_is_bounded_even_for_one_enormous_multibyte_line() {
        let line = "é".repeat(STDERR_TAIL_BYTES); // twice the byte cap
        let tail = stderr_tail(&line);
        assert!(tail.len() <= STDERR_TAIL_BYTES);
        assert!(tail.chars().all(|c| c == 'é'));
        assert_eq!(stderr_tail("a\nb\nc"), "a\nb\nc");
    }
}

/// Builders for stand-in `olmocr` runs, shared by every test that fakes the
/// CLI (here, `event_watch` and `server`).
#[cfg(test)]
pub(crate) mod test_support {
    /// Write `body` as an executable shell script at `path` and return only
    /// once it can be exec'd.
    ///
    /// A script written by one test thread can be inherited, still open for
    /// writing, by a child another thread forks at that moment; until that
    /// child execs, exec'ing the script fails with ETXTBSY ("Text file
    /// busy"). Observed in CI: `spawning olmocr CLI at …/fake-olmocr.sh:
    /// Text file busy (os error 26)`, 4 runs in 60 inside the runner image.
    /// One successful exec proves no writer is left, and none can appear
    /// again (the file is never reopened), so the script is probed with
    /// `--hs-ready` — which it answers by exiting — until an exec succeeds.
    #[cfg(unix)]
    pub(crate) fn install_stand_in(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            format!("#!/bin/sh\n[ \"$1\" = --hs-ready ] && exit 0\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..1000 {
            match std::process::Command::new(path).arg("--hs-ready").status() {
                Ok(status) => {
                    assert!(
                        status.success(),
                        "{}: readiness probe {status}",
                        path.display()
                    );
                    return;
                }
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
        panic!("{}: still busy after 1000 exec attempts", path.display());
    }

    /// The result document olmocr's `build_dolma_document` writes for these
    /// page texts: every page but the last followed by "\n", and each page's
    /// span counted in characters.
    pub(crate) fn result_document(page_texts: &[&str]) -> String {
        let mut text = String::new();
        let mut spans = Vec::with_capacity(page_texts.len());
        let mut pos = 0usize;
        for (i, page) in page_texts.iter().enumerate() {
            text.push_str(page);
            if i + 1 < page_texts.len() {
                text.push('\n');
            }
            let end = text.chars().count();
            spans.push(serde_json::json!([pos, end, i + 1]));
            pos = end;
        }
        serde_json::json!({
            "id": "test",
            "text": text,
            "source": "olmocr",
            "metadata": {},
            "attributes": { "pdf_page_numbers": spans },
        })
        .to_string()
    }

    #[cfg(unix)]
    /// A shell line for a stand-in `olmocr` (its `$1` is the workspace)
    /// that writes `document` where olmocr writes its result.
    pub(crate) fn write_result(document: &str) -> String {
        format!(
            "mkdir -p \"$1/results\" && cat > \"$1/results/output_test.jsonl\" <<'HS_EOF'\n{document}\nHS_EOF"
        )
    }

    #[cfg(unix)]
    /// `n` pages of text, each starting with its own page number.
    pub(crate) fn pages(n: usize, body: &str) -> Vec<String> {
        (1..=n).map(|i| format!("# Page {i}\n\n{body}")).collect()
    }

    #[cfg(unix)]
    /// The completed-page count a stand-in tally reports (at least 1, so a
    /// tally that fails the accounting still has a document behind it).
    /// The tally is shell source (`echo '… Completed pages: 4' >&2`), so the
    /// count is the digits after the marker, not a whitespace-split token.
    pub(crate) fn completed_in(tally: &str) -> usize {
        tally
            .split_once("Completed pages:")
            .map(|(_, rest)| {
                rest.trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0usize)
            .max(1)
    }
}
