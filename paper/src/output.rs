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
