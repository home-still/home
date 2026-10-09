// Weighted Reciprocal Rank Fusion with content relevance and citation signals
use super::dedup::DedupGroup;
use super::merge::contributing_sources;
use super::relevance;
use super::types::RankedPaper;
use crate::models::{Paper, SortBy};

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

/// Impose the caller's requested order on papers already ranked and floored.
/// `Relevance` keeps the blended score order; `Date` and `Citations` are a
/// strict ordering (newest / most cited first, a paper with no value last),
/// ties keeping their blended-score order (the sort is stable).
pub fn order_by(ranked: &mut [RankedPaper], sort_by: &SortBy) {
    match sort_by {
        SortBy::Relevance => {}
        SortBy::Date => ranked.sort_by_key(|rp| std::cmp::Reverse(rp.paper.publication_date)),
        SortBy::Citations => ranked.sort_by_key(|rp| std::cmp::Reverse(rp.paper.cited_by_count)),
    }
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
    fn abstract_match_not_demoted_in_default_ranking() {
        // A paper whose title lacks the query terms but whose abstract matches
        // strongly must keep its high relevance under the DEFAULT ranking — the
        // title-presence floor is a citation-sort-only concern and must not cap
        // ordinary relevance scoring.
        let paper = Paper {
            id: "abs".to_string(),
            title: "A Study of Urban Transit Systems".to_string(),
            authors: Vec::new(),
            abstract_text: Some(
                "retrieval augmented generation improves large language models".to_string(),
            ),
            publication_date: None,
            doi: None,
            download_urls: Vec::new(),
            cited_by_count: Some(10),
            source: "openalex".to_string(),
        };
        let groups = vec![group_of(SourcedPaper {
            paper: paper.clone(),
            rank: 0,
            source: "openalex".to_string(),
        })];
        let ranked = rank_papers(&groups, vec![paper], "retrieval augmented generation");
        assert_eq!(ranked.len(), 1);
        assert!(
            ranked[0].relevance > CITATION_SORT_MIN_RELEVANCE,
            "strong abstract match must not be capped in default ranking, got {}",
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

    fn dated(title: &str, date: Option<(i32, u32, u32)>, cites: Option<u64>) -> Paper {
        let mut p = make_paper(title, "openalex", cites);
        p.publication_date =
            date.map(|(y, m, d)| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap());
        p
    }

    fn ranked_titles(papers: Vec<Paper>, sort_by: &SortBy) -> Vec<String> {
        let groups: Vec<DedupGroup> = papers
            .iter()
            .map(|p| {
                group_of(SourcedPaper {
                    paper: p.clone(),
                    rank: 0,
                    source: "openalex".to_string(),
                })
            })
            .collect();
        let mut ranked = rank_papers(&groups, papers, "graph neural networks");
        order_by(&mut ranked, sort_by);
        ranked.into_iter().map(|rp| rp.paper.title).collect()
    }

    #[test]
    fn date_sort_is_newest_first_with_undated_papers_last() {
        // The old, highly cited, exact-title paper would win a blended
        // ranking; a date sort must still put the newest first.
        let papers = vec![
            dated("graph neural networks", Some((2015, 3, 1)), Some(9_000)),
            dated("graph neural networks survey", None, Some(50_000)),
            dated(
                "graph neural networks revisited",
                Some((2024, 6, 2)),
                Some(1),
            ),
            dated("graph neural networks again", Some((2024, 1, 9)), Some(10)),
        ];
        assert_eq!(
            ranked_titles(papers, &SortBy::Date),
            [
                "graph neural networks revisited",
                "graph neural networks again",
                "graph neural networks",
                "graph neural networks survey"
            ]
        );
    }

    #[test]
    fn citation_sort_is_most_cited_first_with_unknown_counts_last() {
        let papers = vec![
            dated("graph neural networks", Some((2024, 1, 1)), Some(3)),
            dated("graph neural networks survey", Some((2010, 1, 1)), None),
            dated(
                "graph neural networks revisited",
                Some((2012, 1, 1)),
                Some(900),
            ),
            dated("graph neural networks again", Some((2020, 1, 1)), Some(40)),
        ];
        assert_eq!(
            ranked_titles(papers, &SortBy::Citations),
            [
                "graph neural networks revisited",
                "graph neural networks again",
                "graph neural networks",
                "graph neural networks survey"
            ]
        );
    }

    #[test]
    fn relevance_sort_keeps_the_blended_order() {
        let papers = vec![
            dated("ferroptosis in cancer cells", Some((2024, 6, 2)), Some(5)),
            dated("graph neural networks", Some((2010, 1, 1)), Some(5)),
        ];
        assert_eq!(
            ranked_titles(papers, &SortBy::Relevance)[0],
            "graph neural networks"
        );
    }
}
