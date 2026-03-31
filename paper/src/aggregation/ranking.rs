// Weighted Reciprocal Rank Fusion with content relevance and citation signals
use super::dedup::DedupGroup;
use super::merge::contributing_sources;
use super::relevance;
use super::types::RankedPaper;
use crate::models::Paper;

const RRF_K: f64 = 60.0;

// Log(1 + 10_000) - papers with 10k+ citations score near 1.0
const MAX_EXPECTED_CITATIONS: f64 = 10_000.0;

// Per-source weights: how much we trust each provider's relevance ordering
fn source_weight(source: &str) -> f64 {
    match source {
        "semantic_scholar" => 1.0,
        "openalex" => 0.9,
        "arxiv" => 0.8,
        "europe_pmc" => 0.7,
        "crossref" => 0.6,
        "core" => 0.5,
        _ => 0.5,
    }
}

pub fn rank_papers(groups: &[DedupGroup], merged: Vec<Paper>, query: &str) -> Vec<RankedPaper> {
    let mut ranked: Vec<RankedPaper> = groups
        .iter()
        .zip(merged)
        .map(|(group, paper)| {
            // 1. Weighted RRF
            let rrf: f64 = group
                .papers
                .iter()
                .map(|sp| {
                    let w = source_weight(&sp.source);
                    w / (RRF_K + sp.rank as f64 + 1.0)
                })
                .sum();

            // 2. Content relevance (0.0 - 1.0)
            let rel = relevance::relevance_score(query, &paper);

            // 3. Citations boost (0.0 - 1.0, log-scaled)
            let citations = paper.cited_by_count.unwrap_or(0) as f64;
            let citation_boost = (1.0 + citations).ln() / (1.0 + MAX_EXPECTED_CITATIONS).ln();

            // Combined: 40% source concensus, 35% content match, 25% citation impact
            let score = 0.4 * rrf + 0.35 * rel + 0.25 * citation_boost;

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
