use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;

use crate::config::CrossRefConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;

// Search response: message contains items array
#[derive(Debug, Deserialize)]
struct CrSearchResponse {
    message: CrSearchMessage,
}

#[derive(Debug, Deserialize)]
struct CrSearchMessage {
    #[serde(rename = "total-results")]
    total_results: usize,
    items: Vec<CrWork>,
}

// DOI lookup response: message IS the work directly
#[derive(Debug, Deserialize)]
struct CrDoiResponse {
    message: CrWork,
}

#[derive(Debug, Deserialize, Default)]
struct CrWork {
    #[serde(rename = "DOI")]
    doi: Option<String>,
    title: Option<Vec<String>>,
    author: Option<Vec<CrAuthor>>,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    published: Option<CrDate>,
    #[serde(rename = "is-referenced-by-count")]
    cited_by_count: Option<u64>,
    link: Option<Vec<CrLink>>,
}

#[derive(Debug, Deserialize)]
struct CrAuthor {
    given: Option<String>,
    family: Option<String>,
    affiliation: Option<Vec<CrAffiliation>>,
}

#[derive(Debug, Deserialize)]
struct CrAffiliation {
    name: String,
}

#[derive(Debug, Deserialize)]
struct CrDate {
    #[serde(rename = "date-parts")]
    date_parts: Vec<Vec<Option<u32>>>,
}

#[derive(Debug, Deserialize)]
struct CrLink {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "content-type")]
    content_type: Option<String>,
}

pub struct CrossRefProvider {
    client: Client,
    base_url: String,
    mailto: Option<String>,
}

impl CrossRefProvider {
    pub fn new(config: &CrossRefConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: config.base_url.clone(),
            mailto: config.mailto.clone(),
        })
    }

    fn parse_date(date: &CrDate) -> Option<NaiveDate> {
        let parts = date.date_parts.first()?;
        let year = (*parts.first()?)? as i32;
        let month = parts.get(1).and_then(|m| *m).unwrap_or(1);
        let day = parts.get(2).and_then(|d| *d).unwrap_or(1);
        NaiveDate::from_ymd_opt(year, month, day)
    }

    fn cr_work_to_paper(&self, work: CrWork) -> Paper {
        let title = work
            .title
            .and_then(|t| t.into_iter().next())
            .unwrap_or_default();

        let authors = work
            .author
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let name = match (a.given, a.family) {
                    (Some(g), Some(f)) => format!("{} {}", g, f),
                    (None, Some(f)) => f,
                    (Some(g), None) => g,
                    (None, None) => String::new(),
                };
                let affiliations = a
                    .affiliation
                    .unwrap_or_default()
                    .into_iter()
                    .map(|aff| aff.name)
                    .collect();
                Author { name, affiliations }
            })
            .collect();

        let publication_date = work.published.as_ref().and_then(Self::parse_date);

        let mut download_urls = Vec::new();
        if let Some(links) = work.link {
            for link in links {
                if link.content_type.as_deref() == Some("application/pdf") {
                    download_urls.push(link.url);
                }
            }
        }

        Paper {
            id: work.doi.clone().unwrap_or_default(),
            title,
            authors,
            abstract_text: work.abstract_text,
            publication_date,
            doi: work.doi,
            download_urls,
            cited_by_count: work.cited_by_count,
            source: String::from("crossref"),
        }
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let mut params: Vec<(&str, String)> = Vec::new();

        match query.search_type {
            SearchType::Title => params.push(("query.bibliographic", query.query.clone())),
            SearchType::Author => params.push(("query.author", query.query.clone())),
            _ => params.push(("query", query.query.clone())),
        }

        // Pagination
        let rows = query.max_results.min(100);
        params.push(("rows", rows.to_string()));
        params.push(("offset", query.offset.to_string()));

        // Sort
        match query.sort_by {
            SortBy::Date => {
                params.push(("sort", String::from("published")));
                params.push(("order", String::from("desc")));
            }
            SortBy::Citations => {
                params.push(("sort", String::from("is-referenced-by-count")));
                params.push(("order", String::from("desc")));
            }
            SortBy::Relevance => {}
        }

        // Date filter
        if let Some(ref df) = query.date_filter {
            let mut filters = Vec::new();
            if let Some(after) = df.after {
                filters.push(format!("from-pub-date:{}", after.format("%Y-%m-%d")));
            }
            if let Some(before) = df.before {
                let inclusive = before - chrono::Duration::days(1);
                filters.push(format!("until-pub-date:{}", inclusive.format("%Y-%m-%d")));
            }
            if !filters.is_empty() {
                params.push(("filter", filters.join(",")));
            }
        }

        // Polite pool
        if let Some(ref email) = self.mailto {
            params.push(("mailto", email.clone()));
        }

        let base = format!("{}/works", self.base_url);
        let url = url::Url::parse_with_params(&base, &params)
            .map_err(|e| PaperError::InvalidInput(e.to_string()))?;

        Ok(url.to_string())
    }
}

#[async_trait]
impl PaperProvider for CrossRefProvider {
    fn name(&self) -> &'static str {
        "crossref"
    }

    fn priority(&self) -> u8 {
        70
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![
            SearchType::Keywords,
            SearchType::Title,
            SearchType::Author,
            SearchType::DOI,
        ]
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        if matches!(query.search_type, SearchType::DOI) {
            let paper = self.get_by_doi(&query.query).await?;
            return Ok(SearchResult {
                total_results: if paper.is_some() { 1 } else { 0 },
                papers: paper.into_iter().collect(),
                next_offset: None,
                provider: String::from("crossref"),
            });
        }

        let url = self.build_search_url(query)?;
        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(PaperError::RateLimited {
                provider: String::from("crossref"),
                retry_after: None,
            });
        } else if !response.status().is_success() {
            return Err(PaperError::ProviderUnavailable(format!(
                "CrossRef returned {}",
                response.status()
            )));
        }

        let body: CrSearchResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse CrossRef response: {}", e))
        })?;

        let papers: Vec<Paper> = body
            .message
            .items
            .into_iter()
            .map(|w| self.cr_work_to_paper(w))
            .collect();

        let next_offset = query.offset + query.max_results;
        let next_offset = if next_offset < body.message.total_results {
            Some(next_offset)
        } else {
            None
        };

        Ok(SearchResult {
            papers,
            total_results: body.message.total_results,
            next_offset,
            provider: String::from("crossref"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let bare_doi = doi.strip_prefix("https://doi.org/").unwrap_or(doi);

        let mut url = format!("{}/works/{}", self.base_url, bare_doi);
        if let Some(ref email) = self.mailto {
            url.push_str(&format!("?mailto={}", email));
        }

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        } else if !response.status().is_success() {
            return Err(PaperError::ProviderUnavailable(format!(
                "CrossRef returned {}",
                response.status()
            )));
        }

        let body: CrDoiResponse = response
            .json()
            .await
            .map_err(|e| PaperError::ParseError(format!("Failed to parse CrossRef work: {}", e)))?;

        Ok(Some(self.cr_work_to_paper(body.message)))
    }
}
