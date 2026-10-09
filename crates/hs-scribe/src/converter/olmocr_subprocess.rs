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
//!     → mkdir workspace/{markdown,worker_locks,done_flags}
//!     → olmocr workspace --server <endpoint> --model <model>
//!                        --markdown --pdfs <pdf>
//!                        --workers 1 --max_concurrent_requests 4
//!     → read workspace/markdown/<input-mirrored-path>.md
//!     → return assembled markdown
//! ```
//!
//! Concurrency cap rationale: olmocr's default `--max_concurrent_requests`
//! is high enough to overrun vLLM's `--max-num-seqs`, which crashes the
//! APIServer (observed during the manual Phase A backfill). Capping at
//! 4 matches the vLLM serve config on big and stays stable indefinitely.

use std::path::{Path, PathBuf};

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
            "--markdown",
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

    let md_path = find_markdown_output(&workspace_path).context(
        "locating olmocr markdown output in workspace (olmocr did not write the expected file)",
    )?;
    let markdown = tokio::fs::read_to_string(&md_path)
        .await
        .with_context(|| format!("reading olmocr markdown output at `{}`", md_path.display()))?;
    if markdown.trim().is_empty() {
        return Err(anyhow!(
            "olmocr produced an empty markdown file at `{}`",
            md_path.display()
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

/// Walk `workspace/markdown` and return the single `.md` file path.
/// Olmocr mirrors the input PDF's directory tree under `markdown/`, so
/// the exact subpath depends on where the temp file landed. Since we
/// pass exactly one PDF per invocation, there is exactly one output.
fn find_markdown_output(workspace: &Path) -> Result<PathBuf> {
    let markdown_root = workspace.join("markdown");
    if !markdown_root.exists() {
        return Err(anyhow!(
            "workspace/markdown does not exist at `{}`",
            markdown_root.display()
        ));
    }
    let mut stack = vec![markdown_root];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("read_dir on `{}`", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                return Ok(path);
            }
        }
    }
    Err(anyhow!(
        "no .md file found under `{}`",
        workspace.join("markdown").display()
    ))
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
    // script that writes a markdown file and prints the tally olmocr prints.

    #[cfg(unix)]
    fn fake_olmocr(dir: &Path, tally: &str, write_markdown: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-olmocr.sh");
        let body = format!(
            "#!/bin/sh\nws=\"$1\"\n{}\n{tally}\n",
            if write_markdown {
                "mkdir -p \"$ws/markdown\" && printf '# Title\\n\\nbody text' > \"$ws/markdown/out.md\""
            } else {
                ":"
            }
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    async fn run_fake(tally: &str, source_pages: u32) -> Result<String> {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_olmocr(dir.path(), tally, true);
        let pdf = dir.path().join("in.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n").unwrap();
        let config = AppConfig {
            olmocr_bin: script.to_string_lossy().into_owned(),
            ..AppConfig::default()
        };
        convert(&pdf, source_pages, &config).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_complete_run_returns_the_markdown() {
        let md = run_fake(
            "echo 'INFO - Completed pages: 4' >&2; echo 'INFO - Failed pages: 0' >&2",
            4,
        )
        .await
        .unwrap();
        assert!(md.starts_with("# Title"));
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
        assert_eq!(failure_code(&err), Some(FailureCode::OlmocrIncompletePages));
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
        assert_eq!(failure_code(&err), Some(FailureCode::OlmocrIncompletePages));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_with_no_tally_at_all_counts_as_zero_completed_pages() {
        let err = run_fake("echo 'something unrelated' >&2", 4)
            .await
            .unwrap_err();
        assert_eq!(failure_code(&err), Some(FailureCode::OlmocrZeroPages));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_nonzero_exit_is_an_untyped_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("boom.sh");
        std::fs::write(&script, "#!/bin/sh\necho 'vllm unreachable' >&2\nexit 3\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
        std::fs::write(
            &script,
            "#!/bin/sh\necho FIRST-LINE-MARKER >&2\n\
             i=0; while [ $i -lt 5000 ]; do echo \"page $i done\" >&2; i=$((i+1)); done\n\
             echo 'fatal: backend went away' >&2\nexit 3\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
