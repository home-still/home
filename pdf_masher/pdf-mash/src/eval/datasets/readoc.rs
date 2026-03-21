use super::{readoc_dir, GroundTruthSample};
use anyhow::{Context, Result};

/// Load READoc ground truth samples by matching PDFs to ground truth markdown.
/// Expected layout after extraction:
///   readoc/arxiv/pdf/*.pdf + readoc/arxiv_ground_truth/*.md
///   readoc/github/pdf/*.pdf + readoc/github_ground_truth/*.md
pub fn load_readoc_samples(limit: Option<usize>) -> Result<Vec<GroundTruthSample>> {
    let base = readoc_dir();
    let mut samples = Vec::new();
    let max = limit.unwrap_or(usize::MAX);

    for split in &["arxiv", "github"] {
        let pdf_dir = base.join(split).join("pdf");
        let gt_dir = base.join(format!("{}_ground_truth", split));

        if !pdf_dir.exists() || !gt_dir.exists() {
            eprintln!(
                "Skipping READoc split '{}': pdf dir {} or gt dir {} not found",
                split,
                pdf_dir.display(),
                gt_dir.display()
            );
            continue;
        }

        let mut pdf_entries: Vec<_> = std::fs::read_dir(&pdf_dir)
            .with_context(|| format!("Failed to read {}", pdf_dir.display()))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "pdf")
                    .unwrap_or(false)
            })
            .collect();

        pdf_entries.sort_by_key(|e| e.file_name());

        for entry in pdf_entries {
            if samples.len() >= max {
                return Ok(samples);
            }

            let pdf_path = entry.path();
            let stem = pdf_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            let md_path = gt_dir.join(format!("{}.md", stem));
            let markdown = if md_path.exists() {
                std::fs::read_to_string(&md_path).ok()
            } else {
                None
            };

            let doc_id = format!("{}:{}", split, stem);

            samples.push(GroundTruthSample {
                id: doc_id,
                pdf_path,
                image_path: None,
                page_index: None,
                text: None,
                text_blocks: None,
                table_html: None,
                formula_latex: None,
                markdown,
            });
        }
    }

    Ok(samples)
}
