use crate::eval::datasets::GroundTruthSample;
use crate::eval::metrics::bleu::bleu4;
use crate::eval::metrics::composite::{omnidocbench_composite, CompositeScore};
use crate::eval::metrics::edit_distance::normalized_edit_distance;
use crate::ocr::region::RegionType;
use crate::pipeline::processor::Processor;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PageResult {
    pub sample_id: String,
    pub ned: f64,
    pub bleu: f64,
    pub composite: CompositeScoreSerializable,
    pub reference_len: usize,
    pub hypothesis_len: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompositeScoreSerializable {
    pub text_score: Option<f64>,
    pub teds_score: Option<f64>,
    pub cdm_score: Option<f64>,
    pub composite: f64,
}

impl From<CompositeScore> for CompositeScoreSerializable {
    fn from(s: CompositeScore) -> Self {
        Self {
            text_score: s.text_score,
            teds_score: s.teds_score,
            cdm_score: s.cdm_score,
            composite: s.composite,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalResults {
    pub dataset: String,
    pub git_commit: String,
    pub num_samples: usize,
    pub num_failures: usize,
    pub avg_ned: f64,
    pub avg_bleu: f64,
    pub avg_composite: f64,
    pub official_overall: f64,
    pub pages: Vec<PageResult>,
}

/// Result of the parallel OCR phase for a single sample.
struct OcrResult {
    index: usize,
    result: Option<(String, Vec<String>, Option<String>, Option<Vec<String>>)>,
}

/// Run the processor against ground truth samples and score output.
pub async fn run_eval(
    processor: &Processor,
    samples: &[GroundTruthSample],
    dataset_name: &str,
) -> Result<EvalResults> {
    let debug_dir = std::env::var("EVAL_DEBUG_DIR").ok();
    let debug_threshold: f64 = std::env::var("EVAL_DEBUG_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80.0);
    if let Some(ref dir) = debug_dir {
        std::fs::create_dir_all(dir)?;
    }
    let git_commit = get_git_commit();
    let mut pages = Vec::new();
    let mut num_failures: usize = 0;

    let total = samples.len();

    // Phase 1: sequential processing through full pipeline (layout → per-region OCR)
    let mut ocr_results: Vec<OcrResult> = Vec::with_capacity(total);
    for (i, sample) in samples.iter().enumerate() {
        let sample_id = &sample.id;
        eprintln!("[{}/{}] Processing {}...", i + 1, total, sample_id);

        let result = if let Some(ref img_path) = &sample.image_path {
            if !img_path.exists() {
                eprintln!(
                    "FAILED {}: image not found at {}",
                    sample_id,
                    img_path.display()
                );
                None
            } else {
                let t_load = std::time::Instant::now();
                match image::open(img_path) {
                    Ok(image) => {
                        let load_ms = t_load.elapsed().as_millis();
                        let t_proc = std::time::Instant::now();
                        match processor.process_image_full(&image).await {
                            Ok(page) => {
                                let proc_ms = t_proc.elapsed().as_millis();
                                eprintln!("  load={}ms proc={}ms", load_ms, proc_ms);

                                // Build scored components from region types
                                let mut hyp_text_blocks = Vec::new();
                                let mut tables_html = Vec::new();
                                let mut formulas_latex = Vec::new();

                                for region in &page.regions {
                                    let rt = RegionType::from_class(&region.class_name);
                                    let text = region.text.trim();
                                    if text.is_empty() {
                                        continue;
                                    }
                                    match rt {
                                        RegionType::Text => {
                                            hyp_text_blocks.push(text.to_string());
                                        }
                                        RegionType::Table => {
                                            tables_html.push(text.to_string());
                                        }
                                        RegionType::Formula | RegionType::InlineFormula => {
                                            let latex = text
                                                .trim_start_matches("$$")
                                                .trim_end_matches("$$")
                                                .trim_start_matches('$')
                                                .trim_end_matches('$')
                                                .trim()
                                                .to_string();
                                            if !latex.is_empty() {
                                                formulas_latex.push(latex);
                                            }
                                        }
                                        _ => {} // Figure, Skip, FullPage
                                    }
                                }

                                let table_html = if tables_html.is_empty() {
                                    None
                                } else {
                                    Some(tables_html.join("\n"))
                                };
                                let formula_latex = if formulas_latex.is_empty() {
                                    None
                                } else {
                                    Some(formulas_latex)
                                };
                                Some((page.markdown, hyp_text_blocks, table_html, formula_latex))
                            }
                            Err(e) => {
                                eprintln!("FAILED {}: process error: {}", sample_id, e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("FAILED {}: image load error: {}", sample_id, e);
                        None
                    }
                }
            }
        } else {
            eprintln!("FAILED {}: no image path", sample_id);
            None
        };

        ocr_results.push(OcrResult { index: i, result });
    }

    // Phase 2: serial scoring (CPU-bound, fast)
    for ocr_result in &ocr_results {
        let sample = &samples[ocr_result.index];
        let reference = sample
            .markdown
            .as_deref()
            .or(sample.text.as_deref())
            .unwrap_or("");

        let (hypothesis_markdown, hyp_text_blocks, extracted_table_html, extracted_formula_latex) =
            match &ocr_result.result {
                Some(result) => result.clone(),
                None => {
                    num_failures += 1;
                    let has_text_ref = sample.text.is_some();
                    pages.push(PageResult {
                        sample_id: sample.id.clone(),
                        ned: 1.0,
                        bleu: 0.0,
                        composite: CompositeScoreSerializable {
                            text_score: if has_text_ref { Some(0.0) } else { None },
                            teds_score: None,
                            cdm_score: None,
                            composite: 0.0,
                        },
                        reference_len: reference.len(),
                        hypothesis_len: 0,
                    });
                    continue;
                }
            };

        let hypothesis = &hypothesis_markdown;

        let ned = normalized_edit_distance(reference, hypothesis);
        let bleu = bleu4(reference, hypothesis);
        let ref_blocks = sample.text_blocks.as_deref();
        let hyp_blocks = if hyp_text_blocks.is_empty() {
            None
        } else {
            Some(hyp_text_blocks.as_slice())
        };
        // Text is only scored when the reference has text annotations.
        // Pages with only tables/formulas/figures should not penalize text_score.
        let has_text_ref = sample.text.is_some();
        let composite = omnidocbench_composite(
            reference,
            hypothesis,
            ref_blocks,
            hyp_blocks,
            sample.table_html.as_deref(),
            extracted_table_html.as_deref(),
            sample.formula_latex.as_deref().map(|v| v as &[String]),
            extracted_formula_latex.as_deref().map(|v| v as &[String]),
            has_text_ref,
        );

        let composite_ser: CompositeScoreSerializable = composite.into();

        // Save debug text for low-scoring pages
        if let Some(ref dir) = debug_dir {
            if composite_ser.composite < debug_threshold {
                let safe_id = sample.id.replace('/', "_");
                let debug_path = format!("{}/{}.txt", dir, safe_id);
                let debug_content = format!(
                    "=== {} ===\nComposite: {:.1} | NED: {:.3} | text_score: {:.1} | teds: {:?} | cdm: {:?}\nRef len: {} | Hyp len: {}\n\n--- REFERENCE ---\n{}\n\n--- HYPOTHESIS ---\n{}",
                    sample.id,
                    composite_ser.composite,
                    ned,
                    composite_ser.text_score.unwrap_or(f64::NAN),
                    composite_ser.teds_score,
                    composite_ser.cdm_score,
                    reference.len(),
                    hypothesis.len(),
                    reference,
                    hypothesis
                );
                let _ = std::fs::write(&debug_path, &debug_content);
            }
        }

        pages.push(PageResult {
            sample_id: sample.id.clone(),
            ned,
            bleu,
            composite: composite_ser,
            reference_len: reference.len(),
            hypothesis_len: hypothesis.len(),
        });
    }

    let num_samples = pages.len();
    let (avg_ned, avg_bleu, avg_composite) = if num_samples > 0 {
        (
            pages.iter().map(|p| p.ned).sum::<f64>() / num_samples as f64,
            pages.iter().map(|p| p.bleu).sum::<f64>() / num_samples as f64,
            pages.iter().map(|p| p.composite.composite).sum::<f64>() / num_samples as f64,
        )
    } else {
        (0.0, 0.0, 0.0)
    };

    // Average each metric only over pages where it's applicable (Some).
    // This matches official scoring: text only over pages with text annotations,
    // TEDS only over pages with tables, CDM only over pages with formulas.
    let text_pages: Vec<f64> = pages
        .iter()
        .filter_map(|p| p.composite.text_score)
        .collect();
    let avg_text_score = if text_pages.is_empty() {
        0.0
    } else {
        text_pages.iter().sum::<f64>() / text_pages.len() as f64
    };
    let teds_pages: Vec<f64> = pages
        .iter()
        .filter_map(|p| p.composite.teds_score)
        .collect();
    let avg_teds = if teds_pages.is_empty() {
        0.0
    } else {
        teds_pages.iter().sum::<f64>() / teds_pages.len() as f64
    };
    let cdm_pages: Vec<f64> = pages.iter().filter_map(|p| p.composite.cdm_score).collect();
    let avg_cdm = if cdm_pages.is_empty() {
        0.0
    } else {
        cdm_pages.iter().sum::<f64>() / cdm_pages.len() as f64
    };
    let official_overall = (avg_text_score + avg_teds + avg_cdm) / 3.0;

    if num_failures > 0 {
        eprintln!("WARNING: {num_failures}/{num_samples} samples failed and were scored as zero");
    }

    eprintln!(
        "Official v1.5 breakdown: text={:.2} ({} pages) + TEDS={:.2} ({} pages) + CDM={:.2} ({} pages) = Overall {:.2}",
        avg_text_score, text_pages.len(), avg_teds, teds_pages.len(), avg_cdm, cdm_pages.len(), official_overall
    );

    Ok(EvalResults {
        dataset: dataset_name.to_string(),
        git_commit,
        num_samples,
        num_failures,
        avg_ned,
        avg_bleu,
        avg_composite,
        official_overall,
        pages,
    })
}

fn get_git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
