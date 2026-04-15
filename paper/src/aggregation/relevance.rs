use crate::models::Paper;

/// Scores how well a paper matches the query.  Returns 0.0-1.0.
pub fn relevance_score(query: &str, paper: &Paper) -> f64 {
    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

    if query_terms.is_empty() {
        return 0.0;
    }

    let title_lower = paper.title.to_lowercase();
    let abstract_lower = paper.abstract_text.as_deref().unwrap_or("").to_lowercase();
    let haystack = format!("{} {}", title_lower, abstract_lower);

    // 1. Term coverage (40%): fraction of query terms found in title+abstract
    let terms_found = query_terms
        .iter()
        .filter(|term| haystack.contains(**term))
        .count();
    let term_coverage = terms_found as f64 / query_terms.len() as f64;

    // 2. Phrase match boost (30%): full query as contiguous phrase
    let phrase_score = if query_terms.len() > 1 {
        if title_lower.contains(&query_lower) {
            1.0
        } else if abstract_lower.contains(&query_lower) {
            0.5
        } else {
            0.0
        }
    } else if title_lower.contains(&query_lower) {
        1.0
    } else {
        0.0
    };

    // 3. Title match density (20%): query terms / total title words
    let title_words: Vec<&str> = title_lower.split_whitespace().collect();
    let title_hits = query_terms
        .iter()
        .filter(|term| title_lower.contains(**term))
        .count();
    let title_density = if title_words.is_empty() {
        0.0
    } else {
        title_hits as f64 / title_words.len() as f64
    };

    // 4. Metadata completeness (10%)
    let mut meta = 0.0;
    if paper.abstract_text.is_some() {
        meta += 0.5;
    }
    if paper.doi.is_some() {
        meta += 0.25;
    }
    if paper.publication_date.is_some() {
        meta += 0.25;
    }

    // Weighted combination
    0.4 * term_coverage + 0.3 * phrase_score + 0.2 * title_density + 0.1 * meta
}
