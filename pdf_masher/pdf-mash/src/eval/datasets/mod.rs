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
