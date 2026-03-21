// Reciprocal Rank Fusion with boost signals
use super::dedup::DedupGroup;
use super::merge::contributing_sources;
use crate::models::{Paper, RankedPaper};

const RRF_K: f64 = 60.0;

pub fn rank_papers(groups: &[DedupGroup], merged: Vec<Paper>) -> Vec<RankedPaper> {
    let mut ranked: Vec<RankedPaper> = groups
        .iter()
        .zip(merged)
        .map(|(group, paper)| -> _ {
            // Baser RRF score: sum of 1/(k + rank) across all sources
            let score: f64 = group
                .papers
                .iter()
                .map(|sp| 1.0 / (RRF_K + sp.rank as f64 + 1.0))
                .sum();

            RankedPaper {
                contributing_sources: contributing_sources(group),
                score,
                paper,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    ranked
}
