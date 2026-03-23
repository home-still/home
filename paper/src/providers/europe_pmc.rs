use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::Deserialize;

use crate::config::EuropePmcConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::response::check_response;

#[derive(Debug, Deserialize)]
struct EpmcResponse {
    #[serde(rename = "hitCount")]
    hit_count: usize,
    #[serde(rename = "resultList")]
    result_list: EpmcResultList,
}

#[derive(Debug, Deserialize)]
struct EpmcResultList {
    result: Vec<EpmcWork>,
}

#[derive(Debug, Deserialize)]
struct EpmcWork {
    id: String,
    title: Option<String>,
    #[serde(rename = "authorString")]
    author_string: Option<String>,
    #[serde(rename = "abstractText")]
    abstract_text: Option<String>,
    doi: Option<String>,
    #[serde(rename = "firstPublicationDate")]
    first_publication_date: Option<String>,
    #[serde(rename = "citedByCount")]
    cited_by_count: Option<u64>,
    #[serde(rename = "fullTextUrlList")]
    full_text_url_list: Option<EpmcFullTextUrlList>,
}

#[derive(Debug, Deserialize)]
struct EpmcFullTextUrlList {
    #[serde(rename = "fullTextUrl")]
    full_text_url: Vec<EpmcFullTextUrl>,
}

#[derive(Debug, Deserialize)]
struct EpmcFullTextUrl {
    #[serde(rename = "documentStyle")]
    document_style: Option<String>,
    availability: Option<String>,
    url: String,
}

pub struct EuropePmcProvider {
    client: Client,
    base_url: String,
}

impl EuropePmcProvider {
    pub fn new(config: &EuropePmcConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: config.base_url.clone(),
        })
    }

    fn epmc_work_to_paper(&self, work: EpmcWork) -> Paper {
        let authors = work
            .author_string
            .map(|s| {
                s.split(", ")
                    .map(|name| Author {
                        name: name.trim_end_matches('.').to_string(),
                        affiliations: vec![],
                    })
                    .collect()
            })
            .unwrap_or_default();

        let publication_date = work
            .first_publication_date
            .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

        let mut download_urls = Vec::new();
        if let Some(url_list) = work.full_text_url_list {
            for entry in url_list.full_text_url {
                if entry.document_style.as_deref() == Some("pdf") {
                    download_urls.push(entry.url);
                }
            }
        }

        Paper {
            id: work.id,
            title: work.title.unwrap_or_default(),
            authors,
            abstract_text: work.abstract_text,
            publication_date,
            doi: work.doi,
            download_urls,
            cited_by_count: work.cited_by_count,
            source: String::from("europe_pmc"),
        }
    }

    fn build_search_url(&self, query: &SearchQuery) -> String {
        let search_query = match query.search_type {
            SearchType::Title => format!("TITLE:\"{}\"", query.query),
            SearchType::Author => format!("AUTH:\"{}\"", query.query),
            _ => query.query.clone(),
        };

        let search_query = if let Some(ref df) = query.date_filter {
            let from = df
                .after
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "1900-01-01".to_string());
            let to = df
                .before
                .map(|d| {
                    let inclusive = d - chrono::Duration::days(1);
                    inclusive.format("%Y-%m-%d").to_string()
                })
                .unwrap_or_else(|| "2999-12-31".to_string());
            format!("{} FIRST_PDATE:[{} TO {}]", search_query, from, to)
        } else {
            search_query
        };

        let page_size = query.max_results.min(100);

        let sort_param = match query.sort_by {
            SortBy::Date => "&sort=P_PDATE_D+desc",
            SortBy::Citations => "&sort=CITED+desc",
            SortBy::Relevance => "",
        };

        let encoded: String =
            url::form_urlencoded::byte_serialize(search_query.as_bytes()).collect();

        format!("{}/webservices/rest/search?query={}&format=json&resultType=core&pageSize={}&cursorMark=*{}",
          self.base_url, encoded, page_size, sort_param
        )
    }
}

#[async_trait]
impl PaperProvider for EuropePmcProvider {
    fn name(&self) -> &'static str {
        "europe_pmc"
    }

    fn priority(&self) -> u8 {
        75
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
        let url = self.build_search_url(query);
        let response = self.client.get(&url).send().await?;

        check_response(&response, "europe_pmc")?;

        let body: EpmcResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Europe PMC response: {}", e))
        })?;

        let papers: Vec<Paper> = body
            .result_list
            .result
            .into_iter()
            .map(|w| self.epmc_work_to_paper(w))
            .collect();

        let next_offset = query.offset + papers.len();
        let next_offset = if next_offset < body.hit_count {
            Some(next_offset)
        } else {
            None
        };

        Ok(SearchResult {
            papers,
            total_results: body.hit_count,
            next_offset,
            provider: String::from("europe_pmc"),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let bare_doi = doi.strip_prefix("https://doi.org/").unwrap_or(doi);
        let url = format!(
            "{}/webservices/rest/search?query=DOI:{}&format=json&resultType=core",
            self.base_url, bare_doi
        );

        let response = self.client.get(&url).send().await?;

        check_response(&response, "europe_pmc")?;

        let body: EpmcResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Europe PMC response: {}", e))
        })?;

        Ok(body
            .result_list
            .result
            .into_iter()
            .next()
            .map(|w| self.epmc_work_to_paper(w)))
    }
}
