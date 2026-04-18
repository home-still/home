// Weighted Reciprocal Rank Fusion with content relevance and citation signals
use super::dedup::DedupGroup;
use super::merge::contributing_sources;
use super::relevance;
use super::types::RankedPaper;
use crate::models::Paper;

const RRF_K: f64 = 60.0;

// Log(1 + 10_000) - papers with 10k+ citations score near 1.0
const MAX_EXPECTED_CITATIONS: f64 = 10_000.0;

/// When the caller sorts by citations, papers below this content-relevance
/// score are dropped before the final sort. Keeps high-citation off-topic
/// papers from swamping the target paper in "sort by citations" searches.
/// Value chosen to drop papers where only ~30% of query terms match.
pub const CITATION_SORT_MIN_RELEVANCE: f64 = 0.3;

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
                relevance: rel,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregation::dedup::{DedupGroup, MatchType, SourcedPaper};
    use crate::models::Paper;

    fn make_paper(title: &str, source: &str, cites: Option<u64>) -> Paper {
        Paper {
            id: title.to_string(),
            title: title.to_string(),
            authors: Vec::new(),
            abstract_text: None,
            publication_date: None,
            doi: None,
            download_urls: Vec::new(),
            cited_by_count: cites,
            source: source.to_string(),
        }
    }

    fn source_paper(title: &str, source: &str, rank: usize, cites: Option<u64>) -> SourcedPaper {
        SourcedPaper {
            paper: make_paper(title, source, cites),
            rank,
            source: source.to_string(),
        }
    }

    fn group_of(paper: SourcedPaper) -> DedupGroup {
        DedupGroup {
            papers: vec![paper],
            doi: None,
            match_type: MatchType::Single,
        }
    }

    #[test]
    fn relevance_is_stamped_on_ranked_paper() {
        // Title matches every query token → high relevance.
        let groups = vec![group_of(source_paper(
            "retrieval augmented generation",
            "openalex",
            0,
            Some(500),
        ))];
        let merged: Vec<Paper> = groups.iter().map(|g| g.papers[0].paper.clone()).collect();
        let ranked = rank_papers(&groups, merged, "retrieval augmented generation");
        assert_eq!(ranked.len(), 1);
        assert!(
            ranked[0].relevance > 0.5,
            "relevance should be high on full title match, got {}",
            ranked[0].relevance
        );
    }

    #[test]
    fn off_topic_high_citation_paper_has_low_relevance() {
        // Paper about a completely different topic but with many citations.
        let groups = vec![group_of(source_paper(
            "ferroptosis in cancer cells",
            "openalex",
            0,
            Some(1_000),
        ))];
        let merged: Vec<Paper> = groups.iter().map(|g| g.papers[0].paper.clone()).collect();
        let ranked = rank_papers(&groups, merged, "retrieval augmented generation");
        assert_eq!(ranked.len(), 1);
        assert!(
            ranked[0].relevance < CITATION_SORT_MIN_RELEVANCE,
            "off-topic paper should be below the citation-sort floor, got {}",
            ranked[0].relevance
        );
    }

    #[test]
    fn target_paper_survives_citation_floor_even_with_fewer_citations() {
        // The Gao et al. RAG survey (low-citation in this fixture) has
        // perfect relevance; the off-topic high-cite paper does not.
        // After applying the floor, only the target survives.
        let groups = vec![
            group_of(source_paper(
                "retrieval augmented generation for large language models",
                "openalex",
                0,
                Some(400),
            )),
            group_of(source_paper(
                "points of interest recommendation via ranked retrieval",
                "openalex",
                0,
                Some(1_200),
            )),
        ];
        let merged: Vec<Paper> = groups.iter().map(|g| g.papers[0].paper.clone()).collect();
        let ranked = rank_papers(&groups, merged, "retrieval augmented generation");
        let survivors: Vec<&RankedPaper> = ranked
            .iter()
            .filter(|rp| rp.relevance >= CITATION_SORT_MIN_RELEVANCE)
            .collect();
        assert_eq!(survivors.len(), 1, "only the target should survive");
        assert!(
            survivors[0].paper.title.contains("retrieval augmented"),
            "survivor should be the target paper, got: {:?}",
            survivors[0].paper.title
        );
    }
}
