use crate::models::{Paper, SearchResult};
use anyhow::Result;
use serde::Serialize;

use hs_style::styles::Styles;
use owo_colors::OwoColorize;

/// Print any Serialize value as JSON to stdout.
pub fn print_json(value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    println!("{json}");
    Ok(())
}

fn format_authors(paper: &Paper) -> String {
    paper
        .authors
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Print search results as a human-readable list.
pub fn print_search_result(
    result: &SearchResult,
    styles: &Styles,
    show_abstract: bool,
    query: &str,
    offset: usize,
) {
    eprintln!(
        "Found {} results from {} (showing {})\n",
        result.total_results,
        result.provider,
        result.papers.len()
    );

    for (i, paper) in result.papers.iter().enumerate() {
        print_paper_row(i + offset + 1, paper, styles, show_abstract, query);
    }

    if let Some(offset) = result.next_offset {
        if offset < result.total_results {
            eprintln!(
                "\nMore results available. Use --offset {} to see next page.",
                offset
            );
        }
    }
}

/// Format search results as one-line-per-result tab-separated text.
pub fn format_search_result_pipe(result: &SearchResult) -> String {
    let mut out = String::new();
    for paper in &result.papers {
        let authors = format_authors(paper);
        let date = paper
            .publication_date
            .map(|d| d.to_string())
            .unwrap_or_default();
        let doi = paper.doi.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            paper.title, authors, date, doi
        ));
    }
    out
}

/// Print search results as one-line-per-result for piped output.
pub fn print_search_result_pipe(result: &SearchResult) {
    print!("{}", format_search_result_pipe(result));
}

fn print_paper_row(index: usize, paper: &Paper, styles: &Styles, show_abstract: bool, query: &str) {
    let authors = format_authors(paper);
    let date = paper
        .publication_date
        .map(|d| d.to_string())
        .unwrap_or_default();

    println!(
        "{}. {}",
        index,
        highlight_keywords(&paper.title, query, styles)
    );
    println!("   {} ({})", authors, date.style(styles.date));
    print!("   {}", paper.id);
    if let Some(doi) = &paper.doi {
        print!("  doi:{}", doi.style(styles.doi));
    }
    println!();
    if let Some(url) = &paper.download_urls.first() {
        println!("   {}", url.style(styles.url));
    }
    println!();
    if show_abstract {
        if let Some(abs) = &paper.abstract_text {
            println!("   {}", abs);
        }
    }
}

/// Print a single paper in human-readable format.
pub fn print_paper(paper: &Paper, styles: &Styles) {
    let authors = format_authors(paper);
    println!(
        "{} {}",
        "Title:".style(styles.label),
        paper.title.style(styles.title)
    );
    println!("{} {}", "Authors:".style(styles.label), authors);
    if let Some(date) = paper.publication_date {
        println!(
            "{} {}",
            "Date:".style(styles.label),
            date.style(styles.date)
        );
    }
    println!("{} {}", "ID:".style(styles.label), paper.id);
    if let Some(doi) = &paper.doi {
        println!("{} {}", "DOI:".style(styles.label), doi.style(styles.doi));
    }
    if let Some(url) = &paper.download_urls.first() {
        println!("{} {}", "PDF:".style(styles.label), url.style(styles.url));
    }
    if let Some(abs) = &paper.abstract_text {
        println!("\n{}", abs);
    }
}

fn highlight_keywords(text: &str, query: &str, styles: &Styles) -> String {
    let keywords: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
    text.split_whitespace()
        .map(|word| {
            let lower = word.to_lowercase();
            if keywords.iter().any(|kw| lower.contains(kw)) {
                format!("{}", word.style(styles.highlight))
            } else {
                format!("{}", word.style(styles.title))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Author, Paper, SearchResult};
    use chrono::NaiveDate;

    fn make_paper(title: &str, doi: Option<&str>, date: Option<&str>) -> Paper {
        Paper {
            id: "test-id".into(),
            title: title.into(),
            authors: vec![
                Author {
                    name: "Smith J".into(),
                    affiliations: vec![],
                },
                Author {
                    name: "Zhang Y".into(),
                    affiliations: vec![],
                },
            ],
            abstract_text: None,
            publication_date: date.map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap()),
            doi: doi.map(String::from),
            download_urls: vec![],
            cited_by_count: None,
            source: "test".into(),
        }
    }

    fn make_result(papers: Vec<Paper>) -> SearchResult {
        SearchResult {
            total_results: papers.len(),
            next_offset: None,
            provider: "test".into(),
            papers,
        }
    }

    #[test]
    fn pipe_format_tab_separated() {
        let result = make_result(vec![make_paper(
            "CRISPR Review",
            Some("10.1234/test"),
            Some("2024-06-15"),
        )]);
        let out = format_search_result_pipe(&result);
        let fields: Vec<&str> = out.trim().split('\t').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "CRISPR Review");
        assert_eq!(fields[1], "Smith J, Zhang Y");
        assert_eq!(fields[2], "2024-06-15");
        assert_eq!(fields[3], "10.1234/test");
    }

    #[test]
    fn pipe_format_missing_fields() {
        let result = make_result(vec![make_paper("No DOI Paper", None, None)]);
        let out = format_search_result_pipe(&result);
        let fields: Vec<&str> = out.trim().split('\t').collect();
        assert_eq!(fields[2], ""); // missing date
        assert_eq!(fields[3], "-"); // missing doi
    }

    #[test]
    fn pipe_format_multiple_papers() {
        let result = make_result(vec![
            make_paper("Paper One", Some("10.1/a"), Some("2024-01-01")),
            make_paper("Paper Two", Some("10.1/b"), Some("2023-06-15")),
        ]);
        let out = format_search_result_pipe(&result);
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Paper One"));
        assert!(lines[1].starts_with("Paper Two"));
    }
}
