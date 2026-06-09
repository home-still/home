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
//! POST /scribe (PDF bytes) → handle_scribe → convert(...)
//!     → write PDF to /tmp/<uuid>.pdf
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

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use crate::config::AppConfig;

/// Convert a single PDF to markdown via the `olmocr` CLI.
///
/// Errors are wrapped so the server returns HTTP 500 with the chain;
/// the orchestrator's `classify_convert_failure` (Step 2c) decides
/// whether a particular failure is Permanent or Escalate.
pub async fn convert(pdf_bytes: &[u8], config: &AppConfig) -> Result<String> {
    let workspace = tempfile::Builder::new()
        .prefix("hs-scribe-olmocr-")
        .tempdir()
        .context("creating olmocr workspace tempdir")?;
    let pdf_tmp = tempfile::Builder::new()
        .prefix("hs-scribe-olmocr-input-")
        .suffix(".pdf")
        .tempfile()
        .context("creating olmocr input tempfile")?;
    tokio::fs::write(pdf_tmp.path(), pdf_bytes)
        .await
        .context("writing PDF bytes to tempfile")?;

    let workspace_path = workspace.path().to_path_buf();
    let pdf_path = pdf_tmp.path().to_path_buf();

    tracing::info!(
        endpoint = %config.olmocr_endpoint,
        model = %config.olmocr_model,
        workspace = %workspace_path.display(),
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
        .arg(&pdf_path)
        .args(["--workers", "1", "--max_concurrent_requests", "4"])
        // If the handler future is dropped (client disconnect, convert
        // deadline), kill the CLI rather than orphan a process that keeps
        // hammering vLLM for the rest of a 45-minute book.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawning olmocr CLI at `{}`", config.olmocr_bin))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "olmocr CLI exited with status {:?}: {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    // Read olmocr's reported page-failure stats from stdout so the
    // classifier upstream can distinguish "PDF unreadable" (all pages
    // failed) from "convert succeeded" without scanning logs.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (completed, failed) = parse_page_counts(&stdout);
    tracing::info!(completed, failed, "olmocr_subprocess: CLI exited cleanly");
    if completed == 0 {
        // Olmocr produced nothing — pypdfium2 couldn't open the PDF, or
        // its output validation rejected every page (observed on the
        // Russian Code Complete: 45 min of GPU work, then 0/0 counts).
        // This is an OLMOCR-class failure, not proof the PDF is broken:
        // classify_convert_failure treats the unrecognized message as
        // Escalate, so the next backend gets its shot. A genuinely
        // broken PDF dies in seconds there with a real FormatError
        // (Permanent), so the wasted attempt is cheap. Log the CLI's
        // stderr tail for post-mortem since the workspace tempdir is
        // about to be dropped.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_tail: String = stderr
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        tracing::warn!(
            failed,
            stderr_tail = %stderr_tail,
            "olmocr_subprocess: 0 completed pages — escalating to next backend"
        );
        return Err(anyhow!(
            "olmocr reported 0 completed pages (failed={failed}); content may need a different backend"
        ));
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

/// Walk `workspace/markdown` and return the single `.md` file path.
/// Olmocr mirrors the input PDF's directory tree under `markdown/`, so
/// the exact subpath depends on where the temp file landed. Since we
/// pass exactly one PDF per invocation, there is exactly one output.
fn find_markdown_output(workspace: &std::path::Path) -> Result<PathBuf> {
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

/// Parse the `Completed pages: N` / `Failed pages: N` lines olmocr
/// prints to stdout at the end of a run. Returns `(0, 0)` if the
/// markers aren't found (treat as "we don't know" — the downstream
/// empty-markdown check still catches blank output).
fn parse_page_counts(stdout: &str) -> (u64, u64) {
    let mut completed = 0u64;
    let mut failed = 0u64;
    for line in stdout.lines() {
        if let Some(rest) = line.split_once("Completed pages:") {
            if let Some(n) = rest.1.trim().split_whitespace().next() {
                completed = n.parse().unwrap_or(0);
            }
        } else if let Some(rest) = line.split_once("Failed pages:") {
            if let Some(n) = rest.1.trim().split_whitespace().next() {
                failed = n.parse().unwrap_or(0);
            }
        }
    }
    (completed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
