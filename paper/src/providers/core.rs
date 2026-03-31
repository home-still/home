use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;

use crate::config::CoreConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::response::check_response;

#[derive(Debug, Deserialize)]
struct CoreResponse {
    #[serde(rename = "totalHits")]
    total_hits: usize,
    results: Vec<CoreWork>,
}

#[derive(Debug, Deserialize)]
struct CoreWork {
    id: Option<String>,
    title: Option<String>,
    authors: Option<Vec<CoreAuthor>>,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    doi: Option<String>,
    #[serde(rename = "yearPublished")]
    year_published: Option<i32>,
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CoreAuthor {
    name: Option<String>,
}

pub struct CoreProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl CoreProvider {
    pub fn new(config: &CoreConfig) -> Result<Self> {
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

    fn core_work_to_paper(&self, work: CoreWork) -> Paper {
        let authors = work
            .authors
            .unwrap_or_default()
            .into_iter()
            .map(|a| Author {
                name: a.name.unwrap_or_default(),
                affiliations: vec![],
            })
            .collect();

        let publication_date = work
            .year_published
            .and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1));

        let mut download_urls = Vec::new();
        if let Some(url) = work.download_url {
            if !url.is_empty() {
                download_urls.push(url);
            }
        }

        Paper {
            id: work.id.unwrap_or_default(),
            title: work.title.unwrap_or_default(),
            authors,
            abstract_text: work.abstract_text,
            publication_date,
            doi: work.doi,
            download_urls,
            cited_by_count: None,
            source: String::from("core"),
        }
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let q = match query.search_type {
            SearchType::Title => format!("title:\"{}\"", query.query),
            SearchType::Author => format!("authors:\"{}\"", query.query),
            _ => super::query_utils::maybe_quote_phrase(&query.query),
        };

        // Date filter (year-only precision) — must come before q moves into params
        let q = if let Some(ref df) = query.date_filter {
            let from = df.after.map(|d| d.format("%Y").to_string());
            let to = df.before.map(|d| d.format("%Y").to_string());
            match (from, to) {
                (Some(f), Some(t)) => {
                    format!("({}) AND yearPublished>={} AND yearPublished<={}", q, f, t)
                }
                (Some(f), None) => format!("({}) AND yearPublished>={}", q, f),
                (None, Some(t)) => format!("({}) AND yearPublished<={}", q, t),
                (None, None) => q,
            }
        } else {
            q
        };

        let limit = query.max_results.min(100);

        let mut params: Vec<(&str, String)> = vec![
            ("q", q),
            ("limit", limit.to_string()),
            ("offset", query.offset.to_string()),
        ];

        // Sort
        match query.sort_by {
            SortBy::Date => params.push(("sort", String::from("publishedDate:desc"))),
            SortBy::Citations => params.push(("sort", String::from("citationCount:desc"))),
            SortBy::Relevance => {}
        }

        let base = format!("{}/v3/search/works", self.base_url);
        let url = url::Url::parse_with_params(&base, &params)
            .map_err(|e| PaperError::InvalidInput(e.to_string()))?;

        Ok(url.to_string())
    }
}

#[async_trait]
impl PaperProvider for CoreProvider {
    fn name(&self) -> &'static str {
        "core"
    }

    fn priority(&self) -> u8 {
        50
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
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PaperError::ProviderUnavailable(
                "CORE API key required or invalid. Set providers.core.api_key in config.".into(),
            ));
        }
        check_response(&response, "core")?;

        let body: CoreResponse = response
            .json()
            .await
            .map_err(|e| PaperError::ParseError(format!("Failed to parse CORE response: {}", e)))?;

        let papers: Vec<Paper> = body
            .results
            .into_iter()
            .map(|w| self.core_work_to_paper(w))
            .collect();

        let next_offset = query.offset + query.max_results;
        let next_offset = if next_offset < body.total_hits {
            Some(next_offset)
        } else {
            None
        };

        Ok(SearchResult {
            papers,
            total_results: body.total_hits,
            next_offset,
            provider: String::from("core"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let bare_doi = doi.strip_prefix("https://doi.org/").unwrap_or(doi);
        let url = format!(
            "{}/v3/search/works?q=doi:\"{}\"&limit=1",
            self.base_url, bare_doi
        );

        let mut request = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(PaperError::ProviderUnavailable(
                "CORE API key required or invalid. Set providers.core.api_key in config.".into(),
            ));
        }
        check_response(&response, "core")?;

        let body: CoreResponse = response
            .json()
            .await
            .map_err(|e| PaperError::ParseError(format!("Failed to parse CORE response: {}", e)))?;

        Ok(body
            .results
            .into_iter()
            .next()
            .map(|w| self.core_work_to_paper(w)))
    }
}
