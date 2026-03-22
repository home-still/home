// Field-level merging of papers from multiple sources
use super::dedup::DedupGroup;
use crate::models::Paper;

pub fn merge_group(group: &DedupGroup) -> Paper {
    let first = &group.papers[0].paper;

    if group.papers.len() == 1 {
        return first.clone();
    }

    let papers: Vec<&Paper> = group.papers.iter().map(|sp| &sp.paper).collect();
    let id = first.id.clone();
    let title = pick_longest(&papers, |p| &p.title);
    let authors = pick_most_authors(&papers);
    let abstract_text = pick_longest_option(&papers, |p| p.abstract_text.as_deref());
    let publication_date = papers.iter().filter_map(|p| p.publication_date).next();
    let doi = papers
        .iter()
        .filter_map(|p| p.doi.as_deref())
        .next()
        .map(String::from);
    let mut seen = std::collections::HashSet::new();
    let download_urls: Vec<String> = papers
        .iter()
        .flat_map(|p| p.download_urls.iter())
        .filter(|u| seen.insert((*u).clone()))
        .cloned()
        .collect();
    let cited_by_count = papers.iter().filter_map(|p| p.cited_by_count).max();
    let source = contributing_sources(group).join("+");

    Paper {
        id,
        title,
        authors,
        abstract_text,
        publication_date,
        doi,
        download_urls,
        cited_by_count,
        source,
    }
}

pub fn contributing_sources(group: &DedupGroup) -> Vec<String> {
    let mut sources: Vec<String> = group.papers.iter().map(|sp| sp.source.clone()).collect();
    sources.sort();
    sources.dedup();
    sources
}

fn pick_longest<'a>(papers: &[&'a Paper], field: impl Fn(&'a Paper) -> &'a str) -> String {
    papers
        .iter()
        .map(|p| field(p))
        .max_by_key(|s| s.len())
        .unwrap_or_default()
        .to_string()
}

fn pick_longest_option<'a>(
    papers: &[&'a Paper],
    field: impl Fn(&'a Paper) -> Option<&'a str>,
) -> Option<String> {
    papers
        .iter()
        .filter_map(|p| field(p))
        .max_by_key(|s| s.len())
        .map(String::from)
}

fn pick_most_authors(papers: &[&Paper]) -> Vec<crate::models::Author> {
    papers
        .iter()
        .map(|p| &p.authors)
        .max_by_key(|a| a.len())
        .cloned()
        .unwrap_or_default()
}
