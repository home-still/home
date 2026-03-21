// DOI-first deduplication with fuzzy title matching
use crate::models::{DedupStats, Paper};
use std::collections::HashMap;
use strsim::normalized_levenshtein;

pub struct DedupGroup {
    pub papers: Vec<SourcedPaper>,
    pub doi: Option<String>,
    pub match_type: MatchType,
}

pub struct SourcedPaper {
    pub paper: Paper,
    pub source: String,
    pub rank: usize, // position in that source's result list (0-indexed)
}

pub enum MatchType {
    Doi,
    FuzzyTitle { similarity: f64 },
    Single,
}

pub fn deduplicate(source_results: Vec<(String, Vec<Paper>)>) -> (Vec<DedupGroup>, DedupStats) {
    let mut stats = DedupStats::default();
    let mut groups: Vec<DedupGroup> = Vec::new();

    // Flatten into SourcedPapers, preserving rank
    let mut all_papers: Vec<SourcedPaper> = Vec::new();
    for (source, papers) in source_results {
        stats.total_raw += papers.len();
        for (rank, paper) in papers.into_iter().enumerate() {
            all_papers.push(SourcedPaper {
                paper,
                source: source.clone(),
                rank,
            });
        }
    }

    // Stage 1: DOI indexing
    let mut doi_map: HashMap<String, Vec<SourcedPaper>> = HashMap::new();
    let mut no_doi: Vec<SourcedPaper> = Vec::new();

    for sp in all_papers {
        if let Some(ref doi) = sp.paper.doi {
            let key = normalize_doi(doi);
            doi_map.entry(key).or_default().push(sp);
        } else {
            no_doi.push(sp);
        }
    }

    // Stage 2: Build groups from DOI matches
    for (doi, papers) in doi_map {
        if papers.len() > 1 {
            stats.doi_matches += papers.len() - 1;
        }
        let match_type = if papers.len() > 1 {
            MatchType::Doi
        } else {
            MatchType::Single
        };
        groups.push(DedupGroup {
            doi: Some(doi),
            match_type,
            papers,
        });
    }

    // Stage 3: Fuzzy title matching for papers without DOIs
    const FUZZY_THRESHOLD: f64 = 0.85;

    for source_paper in no_doi {
        let source_paper_title = preprocess_title(&source_paper.paper.title);
        let mut matched_idx: Option<(usize, f64)> = None;

        for (i, group) in groups.iter().enumerate() {
            let group_title = preprocess_title(&group.papers[0].paper.title);
            let similarity = normalized_levenshtein(&source_paper_title, &group_title);

            if similarity >= FUZZY_THRESHOLD {
                matched_idx = Some((i, similarity));
                break;
            }
        }

        if let Some((idx, similarity)) = matched_idx {
            groups[idx].match_type = MatchType::FuzzyTitle { similarity };
            groups[idx].papers.push(source_paper);
            stats.fuzzy_matches += 1;
        } else {
            groups.push(DedupGroup {
                papers: vec![source_paper],
                doi: None,
                match_type: MatchType::Single,
            });
        }
    }

    stats.unique = groups.len();
    (groups, stats)
}

fn normalize_doi(doi: &str) -> String {
    doi.strip_prefix("https://doi.org/")
        .unwrap_or(doi)
        .to_lowercase()
}

fn preprocess_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let stripped: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    stripped
        .split_whitespace()
        .filter(|w| !matches!(*w, "the" | "a" | "an"))
        .collect::<Vec<&str>>()
        .join(" ")
}
