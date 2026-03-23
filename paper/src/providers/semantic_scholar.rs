use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;

use crate::config::SemanticScholarConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::response::check_response;

#[derive(Debug, Deserialize)]
struct S2SearchResponse {
    total: usize,
    data: Vec<S2Paper>,
}

#[derive(Debug, Deserialize)]
struct S2Paper {
    #[serde(rename = "paperId")]
    paper_id: String,
    title: Option<String>,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    year: Option<i32>,
    authors: Option<Vec<S2Author>>,
    #[serde(rename = "citationCount")]
    citation_count: Option<u64>,
    #[serde(rename = "externalIds")]
    external_ids: Option<S2ExternalIds>,
    #[serde(rename = "openAccessPdf")]
    open_access_pdf: Option<S2Pdf>,
}

#[derive(Debug, Deserialize)]
struct S2Author {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S2ExternalIds {
    #[serde(rename = "DOI")]
    doi: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S2Pdf {
    url: String,
}

pub struct SemanticScholarProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl SemanticScholarProvider {
    pub fn new(config: &SemanticScholarConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
        })
    }

    fn s2_paper_to_paper(&self, s2: S2Paper) -> Paper {
        let doi = s2.external_ids.and_then(|ids| ids.doi);

        let authors = s2
            .authors
            .unwrap_or_default()
            .into_iter()
            .map(|a| Author {
                name: a.name.unwrap_or_default(),
                affiliations: vec![],
            })
            .collect();

        let publication_date = s2.year.and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1));

        let mut download_urls = Vec::new();
        if let Some(pdf) = s2.open_access_pdf {
            if !pdf.url.is_empty() {
                download_urls.push(pdf.url);
            }
        }

        Paper {
            id: s2.paper_id,
            title: s2.title.unwrap_or_default(),
            authors,
            abstract_text: s2.abstract_text,
            publication_date,
            doi,
            download_urls,
            cited_by_count: s2.citation_count,
            source: String::from("semantic_scholar"),
        }
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let mut params: Vec<(&str, String)> = Vec::new();

        params.push(("query", query.query.clone()));

        // Fields to request
        params.push((
            "fields",
            String::from("title,abstract,externalIds,openAccessPdf,year,authors,citationCount"),
        ));

        // Pagination
        let limit = query.max_results.min(100);
        params.push(("limit", limit.to_string()));
        params.push(("offset", query.offset.to_string()));

        // Sort
        match query.sort_by {
            SortBy::Citations => params.push(("sort", String::from("citationCount:desc"))),
            SortBy::Date => params.push(("sort", String::from("publicationDate:desc"))),
            SortBy::Relevance => {} // default, no param needed
        }

        // Date filter — S2 supports year range
        if let Some(ref df) = query.date_filter {
            let mut range = String::new();
            if let Some(after) = df.after {
                range.push_str(&after.format("%Y").to_string());
            }
            range.push('-');
            if let Some(before) = df.before {
                range.push_str(&before.format("%Y").to_string());
            }
            if range != "-" {
                params.push(("year", range));
            }
        }

        let base = format!("{}/graph/v1/paper/search", self.base_url);
        let url = url::Url::parse_with_params(&base, &params)
            .map_err(|e| PaperError::InvalidInput(e.to_string()))?;

        Ok(url.to_string())
    }
}

#[async_trait]
impl PaperProvider for SemanticScholarProvider {
    fn name(&self) -> &'static str {
        "semantic_scholar"
    }

    fn priority(&self) -> u8 {
        85
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![
            SearchType::Keywords,
            SearchType::Title,
            SearchType::Author,
            SearchType::DOI,
        ]
    }

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        let url = self.build_search_url(query)?;

        let mut request = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("x-api-key", key);
        }

        let response = request.send().await?;

        check_response(&response, "semantic_scholar")?;

        let body: S2SearchResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Semantic Scholar response: {}", e))
        })?;

        let papers: Vec<Paper> = body
            .data
            .into_iter()
            .map(|p| self.s2_paper_to_paper(p))
            .collect();

        let next_offset = query.offset + query.max_results;
        let next_offset = if next_offset < body.total {
            Some(next_offset)
        } else {
            None
        };

        Ok(SearchResult {
            papers,
            total_results: body.total,
            next_offset,
            provider: String::from("semantic_scholar"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let bare_doi = doi.strip_prefix("https://doi.org/").unwrap_or(doi);
        let url = format!(
              "{}/graph/v1/paper/DOI:{}?fields=title,abstract,externalIds,openAccessPdf,year,authors,citationCount",
              self.base_url, bare_doi
          );

        let mut request = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("x-api-key", key);
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_response(&response, "semantic_scholar")?;

        let paper: S2Paper = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Semantic Scholar paper: {}", e))
        })?;

        Ok(Some(self.s2_paper_to_paper(paper)))
    }
}
