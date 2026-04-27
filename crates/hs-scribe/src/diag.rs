//! Per-page conversion diagnostics — opt-in, off by default.
//!
//! When `HS_SCRIBE_DIAG_DIR` is set on the client (event_watch /
//! scribe_cmd), each successful PDF conversion writes
//! `<dir>/<stem>.diag.jsonl` with one line per page plus a final
//! document-summary line. The records exist so the *next* repetition
//! incident can be triaged from a captured artifact instead of guesswork:
//! routing decisions, layout class lists, sampling params on the wire,
//! the first 1 KB of raw VLM output, and the cleanup pass that fired.
//!
//! Records flow server → client over the streaming endpoint
//! (`StreamLine::PageDiag`). Server-only emission was rejected because
//! the QC verdict and post-processing breakdown are computed client-side
//! and need to land in the same file.

use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

/// Per-page diagnostic record. Server-side fields are filled by the
/// processor; client-side fields (`truncation_count_by_pass`, `qc_verdict`)
/// are appended by the document-summary writer at the end of a run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PageDiagRecord {
    pub page_index: usize,
    pub dpi: u16,
    pub image_width: u32,
    pub image_height: u32,
    pub layout_region_count: usize,
    pub layout_region_classes: Vec<String>,
    pub has_tables: bool,
    pub has_formulas: bool,
    /// `"per-region"` for the per-region pipeline, `"empty-page"` for
    /// pages where layout returned zero regions and the empty-page
    /// fallthrough fired (Phase A behavior), `"full-page"` for the
    /// PipelineMode::FullPage explicit operator-chosen path.
    pub routing_path: String,
    pub backend: String,
    /// What was actually sent on the wire — captured post-merge with
    /// backend defaults so it reflects reality, not the Rust struct.
    pub sampling_params: serde_json::Value,
    /// VLM prompt sent for this page (or, in per-region mode, the
    /// prompt for the dominant region — table/formula/text). Diagnostic;
    /// not authoritative for prompt selection logic.
    pub prompt: String,
    /// Total byte length of the raw VLM output (sum across regions for
    /// per-region pipeline).
    pub raw_vlm_output_len: usize,
    /// First 1 KB of the assembled per-page markdown, char-boundary
    /// safe. Big enough to spot a loop pattern, small enough not to
    /// blow up the JSONL.
    pub raw_vlm_output_first_1k: String,
    /// Byte length of the page's assembled markdown (post-region merge,
    /// pre-clean). Equals `raw_vlm_output_len` for full-page mode and
    /// for per-region pipelines without table HTML wrapping.
    pub output_byte_count: usize,
    pub wall_clock_ms: u64,
}

/// Per-pass truncation breakdown from `clean_repetitions`. Sum equals
/// the total truncation count from the legacy single-`usize` return.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TruncationCounts {
    pub char: usize,
    pub ngram4: usize,
    pub ngram2: usize,
    pub ngram1: usize,
}

impl TruncationCounts {
    pub fn total(&self) -> usize {
        self.char + self.ngram4 + self.ngram2 + self.ngram1
    }
}

/// Document-level summary appended after all per-page records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSummaryRecord {
    pub stem: String,
    pub total_pages: usize,
    pub per_page_truncation_counts: Vec<TruncationCounts>,
    pub longest_run_bytes: usize,
    pub qc_verdict: String,
    pub wall_clock_ms: u64,
}

/// Tagged JSONL line. The `kind` field discriminates page records from
/// the trailing document summary so consumers can `jq 'select(.kind == "page")'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagLine {
    Page {
        stem: String,
        #[serde(flatten)]
        record: PageDiagRecord,
    },
    Document(DocSummaryRecord),
}

/// Append-only JSONL writer. Constructed from `HS_SCRIBE_DIAG_DIR`; if
/// the env var is unset or empty the writer is a no-op (`is_none()`
/// branches stay cold in the steady state).
pub struct DiagWriter {
    inner: Option<BufWriter<std::fs::File>>,
}

impl DiagWriter {
    /// Open `<dir>/<stem>.diag.jsonl` for append. Errors are logged and
    /// the writer disables itself — diag is best-effort and must never
    /// take down a real conversion.
    pub fn open(dir: Option<&PathBuf>, stem: &str) -> Self {
        let Some(dir) = dir else {
            return Self { inner: None };
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "diag: failed to create dir; disabling writer for this run"
            );
            return Self { inner: None };
        }
        let path = dir.join(format!("{stem}.diag.jsonl"));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => Self {
                inner: Some(BufWriter::new(f)),
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "diag: failed to open file; disabling writer for this run"
                );
                Self { inner: None }
            }
        }
    }

    /// No-op writer used when diag is disabled or in tests that don't
    /// care about output.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Append a page record. Errors are logged; subsequent writes still
    /// attempted. We never poison the writer on a single failed line.
    pub fn write_page(&mut self, stem: &str, record: PageDiagRecord) {
        let line = DiagLine::Page {
            stem: stem.to_string(),
            record,
        };
        self.write_line(&line);
    }

    pub fn write_document(&mut self, summary: DocSummaryRecord) {
        let line = DiagLine::Document(summary);
        self.write_line(&line);
    }

    fn write_line(&mut self, line: &DiagLine) {
        let Some(w) = self.inner.as_mut() else {
            return;
        };
        match serde_json::to_string(line) {
            Ok(s) => {
                if let Err(e) = writeln!(w, "{s}") {
                    tracing::warn!(error = %e, "diag: writeln failed");
                }
                if let Err(e) = w.flush() {
                    tracing::warn!(error = %e, "diag: flush failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "diag: serialize failed");
            }
        }
    }
}

/// Char-boundary-safe truncation to at most `max_bytes`. Used to cap
/// `raw_vlm_output_first_1k` without panicking on multi-byte codepoints.
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_total_sums_passes() {
        let c = TruncationCounts {
            char: 1,
            ngram4: 2,
            ngram2: 3,
            ngram1: 4,
        };
        assert_eq!(c.total(), 10);
    }

    #[test]
    fn truncate_at_char_boundary_handles_ascii() {
        assert_eq!(truncate_at_char_boundary("hello world", 5), "hello");
        assert_eq!(truncate_at_char_boundary("short", 100), "short");
        assert_eq!(truncate_at_char_boundary("", 10), "");
    }

    #[test]
    fn truncate_at_char_boundary_respects_utf8() {
        // "é" is 2 bytes in UTF-8. Truncating to 1 byte must NOT split it.
        let s = "café";
        let t = truncate_at_char_boundary(s, 4);
        assert_eq!(t, "caf");
        let t = truncate_at_char_boundary(s, 5);
        assert_eq!(t, "café");
        // Boundary at 3 (between 'f' and 'é' first byte): 'caf' returned.
        let t = truncate_at_char_boundary(s, 3);
        assert_eq!(t, "caf");
    }

    #[test]
    fn disabled_writer_is_noop() {
        let mut w = DiagWriter::disabled();
        assert!(!w.is_enabled());
        // Should not panic or crash.
        w.write_page("stem", PageDiagRecord::default());
        w.write_document(DocSummaryRecord {
            stem: "s".into(),
            total_pages: 0,
            per_page_truncation_counts: vec![],
            longest_run_bytes: 0,
            qc_verdict: "Accept".into(),
            wall_clock_ms: 0,
        });
    }

    #[test]
    fn writer_appends_jsonl_and_is_parseable() {
        let dir = std::env::temp_dir().join("hs_scribe_diag_test_writer");
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = DiagWriter::open(Some(&dir), "alpha");
        assert!(w.is_enabled());

        w.write_page(
            "alpha",
            PageDiagRecord {
                page_index: 0,
                dpi: 200,
                routing_path: "per-region".into(),
                backend: "ollama".into(),
                ..Default::default()
            },
        );
        w.write_document(DocSummaryRecord {
            stem: "alpha".into(),
            total_pages: 1,
            per_page_truncation_counts: vec![TruncationCounts::default()],
            longest_run_bytes: 0,
            qc_verdict: "Accept".into(),
            wall_clock_ms: 12345,
        });
        drop(w); // flush BufWriter

        let contents =
            std::fs::read_to_string(dir.join("alpha.diag.jsonl")).expect("diag file written");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: DiagLine = serde_json::from_str(lines[0]).expect("page line parses");
        match first {
            DiagLine::Page { stem, record } => {
                assert_eq!(stem, "alpha");
                assert_eq!(record.dpi, 200);
                assert_eq!(record.routing_path, "per-region");
            }
            other => panic!("expected Page, got {other:?}"),
        }

        let second: DiagLine = serde_json::from_str(lines[1]).expect("doc line parses");
        match second {
            DiagLine::Document(s) => {
                assert_eq!(s.stem, "alpha");
                assert_eq!(s.wall_clock_ms, 12345);
            }
            other => panic!("expected Document, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
