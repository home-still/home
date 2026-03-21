use crate::eval::harness::EvalResults;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// Save evaluation results to a JSON file.
pub fn save_results(results: &EvalResults, output_path: &str) -> Result<()> {
    let parent = Path::new(output_path).parent();
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(results)?;
    std::fs::write(output_path, json)?;
    Ok(())
}

/// Print a summary of evaluation results to stdout.
pub fn print_summary(results: &EvalResults) {
    println!("=== Evaluation Results: {} ===", results.dataset);
    println!("Git commit: {}", results.git_commit);
    println!("Samples evaluated: {}", results.num_samples);
    if results.num_failures > 0 {
        println!(
            "Failures: {} (scored as zero)",
            results.num_failures
        );
    }
    println!("---");
    println!(
        "Avg NED:       {:.4} (lower is better)",
        results.avg_ned
    );
    println!(
        "Avg BLEU-4:    {:.4} (higher is better)",
        results.avg_bleu
    );
    println!(
        "Avg Composite: {:.2} (higher is better, max 100)",
        results.avg_composite
    );
    println!(
        "Official v1.5: {:.2} (text+TEDS+CDM)/3",
        results.official_overall
    );
    println!("---");

    if results.pages.len() <= 20 {
        for page in &results.pages {
            println!(
                "  {} — NED: {:.4}, BLEU: {:.4}, Composite: {:.2}",
                page.sample_id, page.ned, page.bleu, page.composite.composite,
            );
        }
    } else {
        println!("  (showing first 10 of {} pages)", results.pages.len());
        for page in results.pages.iter().take(10) {
            println!(
                "  {} — NED: {:.4}, BLEU: {:.4}, Composite: {:.2}",
                page.sample_id, page.ned, page.bleu, page.composite.composite,
            );
        }
    }
}

/// Print an aggregate summary across multiple datasets.
pub fn print_aggregate_summary(results: &[&EvalResults]) {
    println!("\n=== Aggregate Benchmark Summary ===");
    println!(
        "{:<20} {:>7} {:>10} {:>8} {:>8} {:>8}",
        "Dataset", "Samples", "Composite", "NED", "TEDS", "Failures"
    );
    println!("{}", "-".repeat(65));

    for r in results {
        let avg_teds = compute_avg_teds(r);
        let teds_str = match avg_teds {
            Some(v) => format!("{:.1}", v),
            None => "N/A".to_string(),
        };

        println!(
            "{:<20} {:>7} {:>10.2} {:>8.4} {:>8} {:>8}",
            r.dataset, r.num_samples, r.avg_composite, r.avg_ned, teds_str, r.num_failures
        );
    }
    println!("{}", "-".repeat(65));

    // Weighted average composite across all datasets
    let total_samples: usize = results.iter().map(|r| r.num_samples).sum();
    if total_samples > 0 {
        let weighted_composite: f64 = results
            .iter()
            .map(|r| r.avg_composite * r.num_samples as f64)
            .sum::<f64>()
            / total_samples as f64;
        println!(
            "{:<20} {:>7} {:>10.2}",
            "WEIGHTED AVG", total_samples, weighted_composite
        );
    }
}

/// Compute average TEDS score from page results (returns None if no pages have TEDS).
fn compute_avg_teds(results: &EvalResults) -> Option<f64> {
    let teds_scores: Vec<f64> = results
        .pages
        .iter()
        .filter_map(|p| p.composite.teds_score)
        .collect();
    if teds_scores.is_empty() {
        None
    } else {
        Some(teds_scores.iter().sum::<f64>() / teds_scores.len() as f64)
    }
}

#[derive(Serialize)]
struct AggregateResult {
    datasets: Vec<DatasetSummary>,
    total_samples: usize,
    weighted_composite: f64,
}

#[derive(Serialize)]
struct DatasetSummary {
    dataset: String,
    num_samples: usize,
    num_failures: usize,
    avg_composite: f64,
    avg_ned: f64,
    avg_teds: Option<f64>,
}

/// Save aggregate results across datasets to a JSON file.
pub fn save_aggregate_results(results: &[&EvalResults], output_path: &str) -> Result<()> {
    let parent = Path::new(output_path).parent();
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir)?;
    }

    let total_samples: usize = results.iter().map(|r| r.num_samples).sum();
    let weighted_composite = if total_samples > 0 {
        results
            .iter()
            .map(|r| r.avg_composite * r.num_samples as f64)
            .sum::<f64>()
            / total_samples as f64
    } else {
        0.0
    };

    let aggregate = AggregateResult {
        datasets: results
            .iter()
            .map(|r| DatasetSummary {
                dataset: r.dataset.clone(),
                num_samples: r.num_samples,
                num_failures: r.num_failures,
                avg_composite: r.avg_composite,
                avg_ned: r.avg_ned,
                avg_teds: compute_avg_teds(r),
            })
            .collect(),
        total_samples,
        weighted_composite,
    };

    let json = serde_json::to_string_pretty(&aggregate)?;
    std::fs::write(output_path, json)?;
    Ok(())
}
