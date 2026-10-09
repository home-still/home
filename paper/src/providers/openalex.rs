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
use crate::providers::response::check_response;

/// Fields requested from `/works`: everything `work_to_paper` reads.
const WORK_FIELDS: &str = "id,doi,display_name,publication_date,abstract_inverted_index,authorships,open_access,best_oa_location,locations,cited_by_count";

/// Largest `per_page` OpenAlex's works endpoint accepts.
const OPENALEX_PAGE_MAX: usize = 200;

#[derive(Debug, Deserialize)]
struct OpenAlexResponse {
    meta: Meta,
    results: Vec<Work>,
}

#[allow(dead_code)]
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
    locations: Option<Vec<OaLocation>>,
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
            .and_then(reconstruct_abstract);

        let doi = work
            .doi
            .map(|d| d.strip_prefix("https://doi.org/").unwrap_or(&d).to_string());

        let publication_date = work
            .publication_date
            .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());

        let mut download_urls: Vec<String> = Vec::new();
        // Priority 1: best_oa_location pdf_url
        if let Some(ref loc) = work.best_oa_location {
            if let Some(ref url) = loc.pdf_url {
                download_urls.push(url.clone());
            }
        }

        // Priority 2: open_access.oa_url
        if let Some(ref oa) = work.open_access {
            if let Some(ref url) = oa.oa_url {
                if !download_urls.contains(url) {
                    download_urls.push(url.clone());
                }
            }
        }

        // Priority 3: all other locations' pdf_urls
        if let Some(ref locations) = work.locations {
            for loc in locations {
                if let Some(ref url) = loc.pdf_url {
                    if !download_urls.contains(url) {
                        download_urls.push(url.clone());
                    }
                }
            }
        }

        let cited_by_count = work.cited_by_count;
        let source = String::from("openalex");

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

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let mut params: Vec<(&str, String)> = Vec::new();
        // OpenAlex reads one `filter` parameter: every clause is joined into
        // it with `,`. A repeated `filter` would drop all but one clause.
        let mut filters: Vec<String> = Vec::new();

        // Search vs filter based on search type. Inside a filter value `,`
        // starts the next clause and `|` means OR, so neither may come from
        // the query text.
        let uses_search = matches!(query.search_type, SearchType::Keywords);
        let filter_text = query.query.replace([',', '|'], " ");

        match query.search_type {
            SearchType::Keywords => {
                params.push(("search", query.query.clone()));
            }
            SearchType::Title => filters.push(format!("title.search:{filter_text}")),
            SearchType::Author => filters.push(format!(
                "authorships.author.display_name.search:{filter_text}"
            )),
            SearchType::Subject => {
                filters.push(format!("topics.display_name.search:{filter_text}"))
            }
            _ => params.push(("search", query.query.clone())),
        }

        // Date filter
        if let Some(ref df) = query.date_filter {
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
        }
        if !filters.is_empty() {
            params.push(("filter", filters.join(",")));
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
        let per_page = query.max_results.min(OPENALEX_PAGE_MAX);
        // OpenAlex pages by number, not offset. An offset that is not a whole
        // number of pages would silently return the wrong window.
        let page_size = per_page.max(1);
        if !query.offset.is_multiple_of(page_size) {
            return Err(PaperError::InvalidInput(format!(
                "openalex pages by page number: offset {} must be a multiple of the page size {page_size}",
                query.offset
            )));
        }
        let page = (query.offset / page_size) + 1;
        params.push(("per_page", per_page.to_string()));
        params.push(("page", page.to_string()));

        // Select fields
        params.push(("select", String::from(WORK_FIELDS)));

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

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        let url = self.build_search_url(query)?;
        let response = self.client.get(&url).send().await?;

        check_response(&response, "openalex")?;

        let body: OpenAlexResponse =
            crate::providers::response::parse_json_or_log(response, "openalex").await?;

        let papers: Vec<Paper> = body
            .results
            .into_iter()
            .map(|w| self.work_to_paper(w))
            .collect();

        let next_offset = query
            .offset
            .saturating_add(query.max_results.min(OPENALEX_PAGE_MAX));
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
            provider_failures: Vec::new(),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let doi = crate::stem::normalize_doi(doi)?;

        // `/works/doi:<doi>`: the DOI's `/` separates path segments, anything
        // else reserved inside it is percent-encoded; the key and field list
        // go through the query builder.
        let mut url = url::Url::parse(&self.base_url)
            .map_err(|e| PaperError::InvalidInput(format!("bad OpenAlex base_url: {e}")))?;
        let mut segments = doi.split('/');
        let first = format!("doi:{}", segments.next().unwrap_or_default());
        url.path_segments_mut()
            .map_err(|_| PaperError::InvalidInput("OpenAlex base_url cannot be a base".into()))?
            .pop_if_empty()
            .extend(["works", first.as_str()])
            .extend(segments);
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("select", WORK_FIELDS);
            if let Some(key) = &self.api_key {
                query.append_pair("api_key", key);
            }
        }

        let response = self.client.get(url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_response(&response, "openalex")?;

        let work: Work = response
            .json()
            .await
            .map_err(|e| PaperError::ParseError(format!("Failed to parse OpenAlex work: {}", e)))?;

        Ok(Some(self.work_to_paper(work)))
    }
}

/// Largest word position accepted from an OpenAlex inverted index. Real
/// abstracts are a few hundred words; the cap only bounds the allocation
/// (`MAX_ABSTRACT_POSITION + 1` slots, ~16 MB) against a hostile payload.
const MAX_ABSTRACT_POSITION: usize = 1_000_000;

/// Rebuild the abstract from OpenAlex's `word -> [positions]` index.
///
/// The positions are remote integers, so the word table is sized from a fixed
/// cap, never from the payload: `{"x": [4000000000000]}` would otherwise ask
/// for terabytes or overflow `max + 1`, both of which abort the process. A
/// position above the cap makes the index malformed and yields no abstract. A
/// gap in the positions (a token OpenAlex stripped) renders as a blank, not
/// as a lost abstract.
fn reconstruct_abstract(inverted_index: &HashMap<String, Vec<usize>>) -> Option<String> {
    let Some(max_pos) = inverted_index.values().flatten().copied().max() else {
        return Some(String::new());
    };
    if max_pos > MAX_ABSTRACT_POSITION {
        tracing::warn!(
            max_position = max_pos,
            limit = MAX_ABSTRACT_POSITION,
            "OpenAlex abstract index has an implausible word position; dropping the abstract"
        );
        return None;
    }

    let mut abs = vec![""; max_pos + 1];

    for (word, positions) in inverted_index.iter() {
        for pos in positions {
            abs[*pos] = word;
        }
    }

    Some(abs.join(" "))
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
            result.as_deref(),
            Some("Despite growing interest in the field and in the results")
        );
    }

    #[test]
    fn test_reconstruct_abstract_empty() {
        let index = HashMap::new();
        let result = reconstruct_abstract(&index).expect("an empty index is not malformed");
        assert!(result.trim().is_empty());
    }

    #[test]
    fn hostile_positions_yield_no_abstract_instead_of_aborting() {
        // `usize::MAX + 1` overflowed (release: wrapped to 0 and the next
        // index panicked); 4e12 asked the allocator for ~64 TB. Both abort.
        for pos in [usize::MAX, 4_000_000_000_000, MAX_ABSTRACT_POSITION + 1] {
            let mut index = HashMap::new();
            index.insert("word".to_string(), vec![0, pos]);
            assert_eq!(reconstruct_abstract(&index), None, "position {pos}");
        }
    }

    /// F12: a gap in the positions used to drop the whole abstract.
    #[test]
    fn a_gap_in_positions_renders_as_a_blank() {
        let mut index = HashMap::new();
        index.insert("Hello".to_string(), vec![0]);
        index.insert("world".to_string(), vec![3]);
        assert_eq!(
            reconstruct_abstract(&index).as_deref(),
            Some("Hello   world")
        );
    }

    #[test]
    fn a_position_at_the_cap_is_accepted() {
        let mut index = HashMap::new();
        index.insert("end".to_string(), vec![MAX_ABSTRACT_POSITION]);
        let text = reconstruct_abstract(&index).unwrap();
        assert!(text.ends_with("end"));
        assert_eq!(text.len(), MAX_ABSTRACT_POSITION + 3);
    }

    #[test]
    fn title_search_with_a_date_range_sends_one_filter_param() {
        let provider = OpenAlexProvider::new(&OpenAlexConfig::default()).unwrap();
        let query = SearchQuery {
            query: "graph, networks|x".to_string(),
            search_type: SearchType::Title,
            max_results: 10,
            offset: 0,
            date_filter: Some(crate::models::DateFilter::parse(">=2020 <2022").unwrap()),
            sort_by: SortBy::default(),
            min_citations: None,
        };
        let url = url::Url::parse(&provider.build_search_url(&query).unwrap()).unwrap();
        let filters: Vec<String> = url
            .query_pairs()
            .filter(|(k, _)| k == "filter")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(
            filters,
            vec![
                "title.search:graph  networks x,from_publication_date:2020-01-01,\
                 to_publication_date:2021-12-31"
                    .to_string()
            ]
        );
    }

    fn paged_query(max_results: usize, offset: usize) -> SearchQuery {
        SearchQuery {
            query: "graphs".to_string(),
            search_type: SearchType::Keywords,
            max_results,
            offset,
            date_filter: None,
            sort_by: SortBy::default(),
            min_citations: None,
        }
    }

    #[test]
    fn an_offset_that_is_not_a_whole_page_is_rejected_not_rounded_down() {
        let provider = OpenAlexProvider::new(&OpenAlexConfig::default()).unwrap();
        // 25 / 20 used to become page 2, i.e. offset 20: the wrong window.
        let err = provider
            .build_search_url(&paged_query(20, 25))
            .expect_err("misaligned offset");
        assert!(
            matches!(&err, PaperError::InvalidInput(m)
                if m.contains("openalex") && m.contains("offset 25") && m.contains("multiple of the page size 20")),
            "{err:?}"
        );

        // The page size is the *clamped* one: 500 asked, 200 served.
        let err = provider
            .build_search_url(&paged_query(500, 250))
            .expect_err("misaligned against the clamped page size");
        assert!(
            matches!(&err, PaperError::InvalidInput(m) if m.contains("page size 200")),
            "{err:?}"
        );
    }

    #[test]
    fn an_aligned_offset_selects_its_page() {
        let provider = OpenAlexProvider::new(&OpenAlexConfig::default()).unwrap();
        let page_of = |q: SearchQuery| {
            let url = url::Url::parse(&provider.build_search_url(&q).unwrap()).unwrap();
            url.query_pairs()
                .find(|(k, _)| k == "page")
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(page_of(paged_query(20, 0)).as_deref(), Some("1"));
        assert_eq!(page_of(paged_query(20, 40)).as_deref(), Some("3"));
        assert_eq!(page_of(paged_query(500, 400)).as_deref(), Some("3"));
    }
}
