pub mod fintabnet;
pub mod omnidocbench;
pub mod readoc;

use std::path::{Path, PathBuf};

pub const BENCHMARK_DATA_DIR: &str = "data/benchmarks";

pub fn omnidocbench_dir() -> PathBuf {
    Path::new(BENCHMARK_DATA_DIR).join("omnidocbench")
}

pub fn fintabnet_dir() -> PathBuf {
    Path::new(BENCHMARK_DATA_DIR).join("fintabnet")
}

pub fn readoc_dir() -> PathBuf {
    Path::new(BENCHMARK_DATA_DIR).join("readoc")
}

pub fn dataset_available(dir: &Path) -> bool {
    dir.exists() && dir.is_dir()
}

/// A ground-truth sample for evaluation.
#[derive(Debug, Clone)]
pub struct GroundTruthSample {
    pub id: String,
    pub pdf_path: PathBuf,
    pub image_path: Option<PathBuf>,
    pub page_index: Option<usize>,
    pub text: Option<String>,
    pub text_blocks: Option<Vec<String>>,
    pub table_html: Option<String>,
    pub formula_latex: Option<Vec<String>>,
    pub markdown: Option<String>,
}

/// Refuse a sample whose reference table TEDS cannot score. Such a table
/// would score `None` and silently fall out of the TEDS average, so a
/// dataset that holds one is rejected at load, naming the sample.
pub(crate) fn ensure_reference_table_scorable(
    sample_id: &str,
    table_html: &str,
) -> anyhow::Result<()> {
    match super::metrics::teds::reference_table_problem(table_html) {
        None => Ok(()),
        Some(problem) => {
            anyhow::bail!("reference table of sample {sample_id:?} cannot be scored: {problem}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_table_without_parseable_rows_is_rejected_naming_the_sample() {
        let err = ensure_reference_table_scorable("page_17.jpg", "<table></table>").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("page_17.jpg"), "{msg}");
        assert!(msg.contains("no parseable rows"), "{msg}");
        // One bad table among good ones is still refused.
        let mixed = "<table><tr><td>a</td></tr></table>\n<table><thead></thead></table>";
        let msg = ensure_reference_table_scorable("p2", mixed)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("table 2 of 2"), "{msg}");
        // Text without table markup has no rows; nothing at all has no table.
        assert!(ensure_reference_table_scorable("p3", "no markup here").is_err());
        let msg = ensure_reference_table_scorable("p4", "  ")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no table markup"), "{msg}");
    }

    #[test]
    fn a_reference_table_with_rows_is_accepted() {
        ensure_reference_table_scorable(
            "p",
            "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>",
        )
        .unwrap();
    }
}
