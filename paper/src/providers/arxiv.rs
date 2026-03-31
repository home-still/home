use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use roxmltree;

use crate::config::ArxivConfig;
use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;

pub struct ArxivProvider {
    client: Client,
    base_url: String,
}

impl ArxivProvider {
    pub fn new(config: &ArxivConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: config.base_url.clone(),
        })
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let search_prefix = match query.search_type {
            SearchType::Keywords => "all:",
            SearchType::Title => "ti:",
            SearchType::Author => "au:",
            _ => "all:",
        };

        let search_query = if super::query_utils::is_phrase_query(&query.query) {
            // Send multi-wrd queries as a quoted phrase: all:"autistic female"
            format!("{}\"{}\"", search_prefix, query.query)
        } else {
            // Single word or explicit boolean: split and prefix each term
            query
                .query
                .split_whitespace()
                .map(|term| format!("{}{}", search_prefix, term))
                .collect::<Vec<_>>()
                .join(" AND ")
        };
        let search_query = if let Some(ref df) = query.date_filter {
            let from = df
                .after
                .map(|d| format!("{}000000", d.format("%Y%m%d")))
                .unwrap_or_else(|| "000001010000".to_string());
            let to = df
                .before
                .map(|d| {
                    let day_before = d - chrono::Duration::days(1);
                    format!("{}235959", day_before.format("%Y%m%d"))
                })
                .unwrap_or_else(|| "999912312359".to_string());
            format!("{} AND submittedDate:[{} TO {}]", search_query, from, to)
        } else {
            search_query
        };

        let (sort_by, sort_order) = match query.sort_by {
            SortBy::Relevance => ("relevance", "descending"),
            SortBy::Citations => ("relevance", "descending"), // TODO: arXiv has no citation sort
            SortBy::Date => ("submittedDate", "descending"),
        };
        let url = url::Url::parse_with_params(
            &self.base_url,
            &[
                ("search_query", search_query.as_str()),
                ("start", &query.offset.to_string()),
                ("max_results", &query.max_results.to_string()),
                ("sortBy", sort_by),
                ("sortOrder", sort_order),
            ],
        )
        .map_err(|e| PaperError::InvalidInput(e.to_string()))?;

        Ok(url.to_string())
    }

    fn extract_paper(&self, entry: roxmltree::Node, ns: &str) -> Result<Paper, PaperError> {
        let id = entry
            .children()
            .find(|n| n.has_tag_name((ns, "id")))
            .and_then(|n| n.text())
            .ok_or_else(|| PaperError::ParseError("Missing ID".into()))?;

        // Extract just the arxiv ID from the full URL.
        // e.g., "http://arxiv.org/abs/1234.5678v1" -> "1234.5678v1"
        let short_id = id.rsplit("/").next().unwrap_or(id);

        let title = entry
            .children()
            .find(|n| n.has_tag_name((ns, "title")))
            .and_then(|n| n.text())
            .ok_or_else(|| PaperError::ParseError("Missing title".into()))?
            .trim()
            .to_string();

        let authors: Vec<crate::models::Author> = entry
            .children()
            .filter(|n| n.has_tag_name((ns, "author")))
            .filter_map(|author_node| {
                author_node
                    .children()
                    .find(|n| n.has_tag_name((ns, "name")))
                    .and_then(|n| n.text())
                    .map(|name| crate::models::Author {
                        name: name.to_string(),
                        affiliations: vec![],
                    })
            })
            .collect();

        let abstract_text = entry
            .children()
            .find(|n| n.has_tag_name((ns, "summary")))
            .and_then(|n| n.text())
            .map(|s| s.trim().to_string());

        let publication_date = entry
            .children()
            .find(|n| n.has_tag_name((ns, "published")))
            .and_then(|n| n.text())
            .and_then(|s| chrono::NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d").ok());

        let download_url = entry
            .children()
            .filter(|n| n.has_tag_name((ns, "link")))
            .find(|n| n.attribute("title") == Some("pdf"))
            .and_then(|n| n.attribute("href"))
            .map(String::from);

        let doi = entry
            .children()
            .find(|n| n.has_tag_name(("http://arxiv.org/schemas/atom", "doi")))
            .and_then(|n| n.text())
            .map(|s| s.trim().to_string());

        Ok(Paper {
            id: String::from(short_id),
            title,
            authors,
            abstract_text,
            publication_date,
            doi,
            download_urls: download_url.into_iter().collect(),
            source: String::from("arxiv"),
            cited_by_count: None,
        })
    }

    fn parse_atom_feed(&self, xml: &str) -> Result<(Vec<Paper>, usize), PaperError> {
        let doc =
            roxmltree::Document::parse(xml).map_err(|e| PaperError::ParseError(e.to_string()))?;

        let root = doc.root_element();
        let ns = "http://www.w3.org/2005/Atom";
        let opensearch_ns = "http://a9.com/-/spec/opensearch/1.1/";

        let total_results = root
            .children()
            .find(|n| n.has_tag_name((opensearch_ns, "totalResults")))
            .and_then(|n| n.text())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);

        let papers: Vec<Paper> = root
            .children()
            .filter(|n| n.has_tag_name((ns, "entry")))
            .filter_map(|entry| self.extract_paper(entry, ns).ok())
            .collect();

        Ok((papers, total_results))
    }
}

#[async_trait]
impl PaperProvider for ArxivProvider {
    fn name(&self) -> &'static str {
        "arxiv"
    }

    fn priority(&self) -> u8 {
        80 // High priority for CS/Physics papers
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![SearchType::Keywords, SearchType::Title, SearchType::Author]
    }

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        let url = self.build_search_url(query)?;

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(PaperError::RateLimited {
                provider: String::from("arxiv"),
                retry_after: None,
            });
        } else if !response.status().is_success() {
            return Err(PaperError::ProviderUnavailable(format!(
                "arXiv returned {}",
                response.status()
            )));
        }

        let xml = response.text().await?;
        let (papers, total_results) = self.parse_atom_feed(&xml)?;

        Ok(SearchResult {
            papers,
            total_results,
            next_offset: Some(query.offset + query.max_results),
            provider: String::from("arxiv"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let query = SearchQuery {
            query: String::from(doi),
            search_type: SearchType::DOI,
            max_results: 1,
            offset: 0,
            date_filter: None,
            sort_by: SortBy::default(),
            min_citations: None,
        };

        let result = self.search_by_query(&query).await?;
        Ok(result.papers.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ArxivConfig;
    use crate::models::SearchType;

    fn provider() -> ArxivProvider {
        ArxivProvider::new(&ArxivConfig::default()).expect("Failed to create provider")
    }

    #[test]
    fn test_build_search_url_title() {
        let p = provider();
        let query = SearchQuery {
            query: String::from("neural networks"),
            search_type: SearchType::Title,
            max_results: 10,
            offset: 0,
            date_filter: None,
            sort_by: SortBy::default(),
            min_citations: None,
        };
        let url = p.build_search_url(&query).expect("Failed to build URL");
        assert!(url.contains("search_query=ti%3A"));
        assert!(url.contains("max_results=10"));
        assert!(url.contains("start=0"));
    }

    #[test]
    fn test_parse_atom_feed_extracts_paper() {
        let p = provider();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
              <feed xmlns="http://www.w3.org/2005/Atom"
                    xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/"
                    xmlns:arxiv="http://arxiv.org/schemas/atom">
                  <opensearch:totalResults>1</opensearch:totalResults>
                  <entry>
                      <id>http://arxiv.org/abs/2301.00001v1</id>
                      <title>Test Paper Title</title>
                      <author><name>Alice Smith</name></author>
                      <author><name>Bob Jones</name></author>
                      <summary>This is the abstract.</summary>
                      <published>2023-01-15T00:00:00Z</published>
                      <link href="http://arxiv.org/pdf/2301.00001v1" title="pdf" rel="related" type="application/pdf"/>
                      <arxiv:doi>10.1234/test.doi</arxiv:doi>
                  </entry>
              </feed>"#;

        let (papers, total) = p.parse_atom_feed(xml).expect("Failed to parse");
        assert_eq!(total, 1);
        assert_eq!(papers.len(), 1);

        let paper = &papers[0];
        assert_eq!(paper.id, "2301.00001v1");
        assert_eq!(paper.title, "Test Paper Title");
        assert_eq!(paper.authors.len(), 2);
        assert_eq!(paper.authors[0].name, "Alice Smith");
        assert_eq!(
            paper.abstract_text.as_deref(),
            Some("This is the abstract.")
        );
        assert_eq!(
            paper.publication_date,
            Some(chrono::NaiveDate::from_ymd_opt(2023, 1, 15).expect("Invalid date"))
        );
        assert_eq!(
            paper.download_urls.first().map(|s| s.as_str()),
            Some("http://arxiv.org/pdf/2301.00001v1")
        );
        assert_eq!(paper.doi.as_deref(), Some("10.1234/test.doi"));
        assert_eq!(paper.source, "arxiv");
    }

    #[test]
    fn test_parse_atom_feed_empty() {
        let p = provider();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
              <feed xmlns="http://www.w3.org/2005/Atom"
                    xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/">
                  <opensearch:totalResults>0</opensearch:totalResults>
              </feed>"#;

        let (papers, total) = p.parse_atom_feed(xml).expect("Failed to parse");
        assert_eq!(total, 0);
        assert!(papers.is_empty());
    }
}
