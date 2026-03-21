use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;

use crate::config::OpenAlexConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;

#[derive(Debug, Deserialize)]
struct OpenAlexResponse {
    meta: Meta,
    results: Vec<Work>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    count: usize,
    per_page: usize,
    page: usize,
}

#[derive(Debug, Deserialize, Default)]
struct Work {
    id: String,
    doi: Option<String>,
    display_name: Option<String>,
    publication_date: Option<String>,
    abstract_inverted_index: Option<HashMap<String, Vec<usize>>>,
    authorships: Option<Vec<Authorship>>,
    open_access: Option<OpenAccess>,
    best_oa_location: Option<OaLocation>,
    cited_by_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Authorship {
    author: AuthorRef,
    institutions: Option<Vec<Institution>>,
}

#[derive(Debug, Deserialize)]
struct AuthorRef {
    display_name: Option<String>,
}

impl Default for AuthorRef {
    fn default() -> Self {
        Self {
            display_name: Some(String::from("Unknown")),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Institution {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAccess {
    oa_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaLocation {
    pdf_url: Option<String>,
}

pub struct OpenAlexProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAlexProvider {
    pub fn new(config: &OpenAlexConfig) -> Result<Self> {
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

    fn work_to_paper(&self, work: Work) -> Paper {
        let id = work
            .id
            .strip_prefix("https://openalex.org/")
            .unwrap_or(&work.id)
            .to_string();

        let title = work.display_name.unwrap_or_default();
        let authors = work
            .authorships
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let affiliations = a
                    .institutions
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|inst| inst.display_name)
                    .collect::<Vec<String>>();

                Author {
                    name: a.author.display_name.unwrap_or_default(),
                    affiliations,
                }
            })
            .collect();

        let abstract_text = work
            .abstract_inverted_index
            .filter(|idx| !idx.is_empty())
            .as_ref()
            .map(reconstruct_abstract);

        let doi = work
            .doi
            .map(|d| d.strip_prefix("https://doi.org/").unwrap_or(&d).to_string());

        let publication_date = work
            .publication_date
            .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

        let download_url = work
            .best_oa_location
            .and_then(|loc| loc.pdf_url)
            .or_else(|| work.open_access.and_then(|oa| oa.oa_url));

        let cited_by_count = work.cited_by_count;
        let source = String::from("openalex");

        Paper {
            id,
            title,
            authors,
            abstract_text,
            publication_date,
            doi,
            download_url,
            cited_by_count,
            source,
        }
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        // TODO: Build the OpenAlex API URL
        let mut params: Vec<(&str, String)> = Vec::new();

        // Search vs filter based on search type
        let uses_search = matches!(query.search_type, SearchType::Keywords);

        match query.search_type {
            SearchType::Keywords => {
                params.push(("search", query.query.clone()));
            }
            SearchType::Title => params.push(("filter", format!("title.search:{}", query.query))),
            SearchType::Author => params.push((
                "filter",
                format!("authorships.author.display_name.search:{}", query.query),
            )),
            SearchType::Subject => {
                params.push((
                    "filter",
                    format!("topics.display_name.search:{}", query.query),
                ));
            }
            _ => params.push(("search", query.query.clone())),
        }

        // Date filter
        if let Some(ref df) = query.date_filter {
            let mut filters = Vec::new();
            if let Some(after) = df.after {
                filters.push(format!(
                    "from_publication_date:{}",
                    after.format("%Y-%m-%d")
                ));
            }
            if let Some(before) = df.before {
                let inclusive = before - chrono::Duration::days(1);
                filters.push(format!(
                    "to_publication_date:{}",
                    inclusive.format("%Y-%m-%d")
                ));
            }
            if !filters.is_empty() {
                params.push(("filter", filters.join(",")));
            }
        }

        // Sort
        let sort = match query.sort_by {
            SortBy::Relevance if uses_search => Some("relevance_score:desc"),
            SortBy::Relevance => None,
            SortBy::Date => Some("publication_date:desc"),
            SortBy::Citations => Some("cited_by_count:desc"),
        };
        if let Some(s) = sort {
            params.push(("sort", s.to_string()));
        }

        // Pagination
        let per_page = query.max_results.min(200);
        let page = (query.offset / query.max_results.max(1)) + 1;
        params.push(("per_page", per_page.to_string()));
        params.push(("page", page.to_string()));

        // Select fields
        params.push(("select", String::from("id,doi,display_name,publication_date,abstract_inverted_index,authorships,open_access,best_oa_location,cited_by_count")));

        // API key
        if let Some(ref key) = self.api_key {
            params.push(("api_key", key.clone()));
        }

        let base = format!("{}/works", self.base_url);
        let url = url::Url::parse_with_params(&base, &params)
            .map_err(|e| PaperError::InvalidInput(e.to_string()))?;

        Ok(url.to_string())
    }
}

#[async_trait]
impl PaperProvider for OpenAlexProvider {
    fn name(&self) -> &'static str {
        "openalex"
    }

    fn priority(&self) -> u8 {
        90
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![
            SearchType::Keywords,
            SearchType::Title,
            SearchType::Author,
            SearchType::DOI,
            SearchType::Subject,
        ]
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        if matches!(query.search_type, SearchType::DOI) {
            let paper = self.get_by_doi(&query.query).await?;
            return Ok(SearchResult {
                total_results: if paper.is_some() { 1 } else { 0 },
                papers: paper.into_iter().collect(),
                next_offset: None,
                provider: String::from("openalex"),
            });
        }

        let url = self.build_search_url(query)?;
        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(std::time::Duration::from_secs);

            return Err(PaperError::RateLimited {
                provider: String::from("openalex"),
                retry_after,
            });
        } else if !response.status().is_success() {
            return Err(PaperError::ProviderUnavailable(format!(
                "OpenAlex returned {}",
                response.status()
            )));
        }

        let body: OpenAlexResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse OpenAlex response: {}", e))
        })?;

        let papers: Vec<Paper> = body
            .results
            .into_iter()
            .map(|w| self.work_to_paper(w))
            .collect();

        let next_offset = query.offset + query.max_results;
        let next_offset = if next_offset < body.meta.count && next_offset < 10_000 {
            Some(next_offset)
        } else {
            None
        };

        Ok(SearchResult {
            papers,
            total_results: body.meta.count,
            next_offset,
            provider: String::from("openalex"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let bare_doi = doi.strip_prefix("https://doi.org/").unwrap_or(doi);

        let mut url = format!("{}/works/doi:{}?select=id,doi,display_name,publication_date,abstract_inverted_index,authorships,open_access,best_oa_location,cited_by_count",
          self.base_url, bare_doi
        );

        if let Some(ref key) = self.api_key {
            url.push_str(&format!("&api_key={}", key));
        }

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        } else if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            return Err(PaperError::RateLimited {
                provider: String::from("openalex"),
                retry_after,
            });
        } else if !response.status().is_success() {
            return Err(PaperError::ProviderUnavailable(format!(
                "OpenAlex returned {}",
                response.status()
            )));
        }

        let work: Work = response
            .json()
            .await
            .map_err(|e| PaperError::ParseError(format!("Failed to parse OpenAlex work: {}", e)))?;

        Ok(Some(self.work_to_paper(work)))
    }
}

fn reconstruct_abstract(inverted_index: &HashMap<String, Vec<usize>>) -> String {
    let max_pos = inverted_index
        .values()
        .map(|positions| positions.iter().max().unwrap_or(&0_usize))
        .max()
        .unwrap_or(&0_usize);

    let mut abs = vec![""; max_pos + 1];

    for (word, positions) in inverted_index.iter() {
        for pos in positions {
            abs[*pos] = word;
        }
    }

    abs.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconstruct_abstract() {
        let mut index = HashMap::new();
        index.insert("Despite".to_string(), vec![0]);
        index.insert("growing".to_string(), vec![1]);
        index.insert("interest".to_string(), vec![2]);
        index.insert("in".to_string(), vec![3, 7]);
        index.insert("the".to_string(), vec![4, 8]);
        index.insert("field".to_string(), vec![5]);
        index.insert("and".to_string(), vec![6]);
        index.insert("results".to_string(), vec![9]);
        let result = reconstruct_abstract(&index);
        assert_eq!(
            result,
            "Despite growing interest in the field and in the results"
        );
    }

    #[test]
    fn test_reconstruct_abstract_empty() {
        let index = HashMap::new();
        let result = reconstruct_abstract(&index);
        assert!(result.trim().is_empty());
    }
}
