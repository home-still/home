use super::{omnidocbench_dir, GroundTruthSample};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Language filter for OmniDocBench samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageFilter {
    All,
    English,
}

/// Filter criteria for OmniDocBench samples.
#[derive(Debug, Clone, Default)]
pub struct SampleFilter {
    pub language: Option<LanguageFilter>,
    pub data_source: Option<String>,
    pub require_tables: bool,
    pub require_formulas: bool,
}

/// Load OmniDocBench ground truth samples from the JSON annotation file.
/// Expected layout: data/benchmarks/omnidocbench/OmniDocBench.json + images/
pub fn load_omnidocbench_samples(
    limit: Option<usize>,
    language: LanguageFilter,
) -> Result<Vec<GroundTruthSample>> {
    load_omnidocbench_filtered(
        limit,
        &SampleFilter {
            language: Some(language),
            ..Default::default()
        },
    )
}

pub fn load_omnidocbench_filtered(
    limit: Option<usize>,
    filter: &SampleFilter,
) -> Result<Vec<GroundTruthSample>> {
    let base = omnidocbench_dir();
    let json_path = base.join("OmniDocBench.json");

    let content = std::fs::read_to_string(&json_path)
        .with_context(|| format!("Failed to read {}", json_path.display()))?;

    let root: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse OmniDocBench JSON")?;

    let entries = root.as_array().context("Expected top-level JSON array")?;

    let mut samples = Vec::new();
    let max = limit.unwrap_or(usize::MAX);

    for entry in entries {
        if samples.len() >= max {
            break;
        }

        let page_info = &entry["page_info"];

        // Language filter
        let language = filter.language.unwrap_or(LanguageFilter::All);
        if language == LanguageFilter::English {
            let lang = page_info["page_attribute"]["language"]
                .as_str()
                .unwrap_or("");
            if lang != "english" {
                continue;
            }
        }

        // Data source filter
        if let Some(ref required_source) = filter.data_source {
            let source = page_info["page_attribute"]["data_source"]
                .as_str()
                .unwrap_or("");
            if source != required_source.as_str() {
                continue;
            }
        }

        let image_name = page_info["image_path"].as_str().unwrap_or("").to_string();
        let page_idx = page_info["page_no"].as_u64().map(|n| n as usize);

        let image_path = resolve_image_path(&base, &image_name);

        let (text, text_blocks) = extract_text_annotations(entry);
        let table_html = extract_table_annotations(entry);
        let formula_latex = extract_formula_annotations(entry);

        // Require tables/formulas if requested
        if filter.require_tables && table_html.is_none() {
            continue;
        }
        if filter.require_formulas && formula_latex.is_none() {
            continue;
        }

        samples.push(GroundTruthSample {
            id: image_name.clone(),
            pdf_path: PathBuf::new(), // OmniDocBench doesn't provide PDFs
            image_path: Some(image_path),
            page_index: page_idx,
            text,
            text_blocks,
            table_html,
            formula_latex,
            markdown: None,
        });
    }

    Ok(samples)
}

fn resolve_image_path(base: &Path, image_name: &str) -> PathBuf {
    let candidate = base.join("images").join(image_name);
    if candidate.exists() {
        return candidate;
    }
    // Fallback: try directly in base
    base.join(image_name)
}

fn extract_text_annotations(entry: &serde_json::Value) -> (Option<String>, Option<Vec<String>>) {
    let annotations = match entry["layout_dets"].as_array() {
        Some(a) => a,
        None => return (None, None),
    };
    let mut items: Vec<(i64, String)> = Vec::new();
    for ann in annotations {
        let category = ann["category_type"].as_str().unwrap_or("");
        // Official v1.5 scoreable text categories only.
        // Caption/header/footer categories are "ignore" in official scoring.
        // NOTE: Including captions in ref was tested and helped vs excluding them,
        // but aligning with official scoring (scoreable-only) may be more accurate.
        if matches!(
            category,
            "text_block"
                | "title"
                | "reference"
                | "figure_caption"
                | "table_caption"
                | "table_footnote"
                | "formula_caption"
                | "equation_caption"
                | "code_txt"
                | "code_txt_caption"
        ) {
            if let Some(t) = ann["text"].as_str() {
                let order = ann["order"].as_i64().unwrap_or(i64::MAX);
                items.push((order, t.to_string()));
            }
        }
        // equation_isolated is NOT included in text — scored only via CDM
    }
    if items.is_empty() {
        (None, None)
    } else {
        items.sort_by_key(|(order, _)| *order);
        let blocks: Vec<String> = items.iter().map(|(_, t)| t.clone()).collect();
        let joined = items
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join("\n");
        (Some(joined), Some(blocks))
    }
}

fn extract_table_annotations(entry: &serde_json::Value) -> Option<String> {
    let annotations = entry["layout_dets"].as_array()?;
    let mut entries: Vec<(i64, String)> = Vec::new();
    for ann in annotations {
        if ann["category_type"].as_str() == Some("table") {
            if let Some(html) = ann["html"].as_str() {
                let order = ann["order"].as_i64().unwrap_or(i64::MAX);
                entries.push((order, html.to_string()));
            }
        }
    }
    if entries.is_empty() {
        None
    } else {
        entries.sort_by_key(|(order, _)| *order);
        Some(
            entries
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn extract_formula_annotations(entry: &serde_json::Value) -> Option<Vec<String>> {
    let annotations = entry["layout_dets"].as_array()?;
    let mut entries: Vec<(i64, String)> = Vec::new();
    for ann in annotations {
        if ann["category_type"].as_str() == Some("equation_isolated") {
            if let Some(latex) = ann["latex"].as_str() {
                let order = ann["order"].as_i64().unwrap_or(i64::MAX);
                entries.push((order, latex.to_string()));
            }
        }
    }
    if entries.is_empty() {
        None
    } else {
        entries.sort_by_key(|(order, _)| *order);
        Some(entries.into_iter().map(|(_, s)| s).collect())
    }
}
