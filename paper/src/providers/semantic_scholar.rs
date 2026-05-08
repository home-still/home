use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::SemanticScholarConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::downloader::strip_arxiv_doi_prefix;
use crate::providers::response::{check_response, send_with_429_retry};

/// Choose the Semantic Scholar identifier prefix for a DOI input. SS does not
/// index DataCite-synthesized arXiv DOIs (`10.48550/arXiv.X`) under its `DOI:`
/// route — those papers are addressable only via the `ARXIV:X` form. The
/// home-still pipeline produces arXiv DOIs (`resolve_doi` synthesizes them),
/// so without this routing every arXiv-only paper would 404.
fn ss_identifier_for_doi(doi: &str) -> String {
    let bare = doi.strip_prefix("https://doi.org/").unwrap_or(doi);
    match strip_arxiv_doi_prefix(bare) {
        Some(arxiv_id) => format!("ARXIV:{arxiv_id}"),
        None => format!("DOI:{bare}"),
    }
}

#[derive(Debug, Deserialize)]
struct S2SearchResponse {
    total: usize,
    data: Vec<S2Paper>,
}

#[derive(Debug, Deserialize)]
struct S2Paper {
    // SS returns `paperId: null` for thin citation records (papers it knows
    // about by external ID but has not assigned an internal ID to yet).
    #[serde(rename = "paperId", default)]
    paper_id: Option<String>,
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
    #[serde(default)]
    venue: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S2Author {
    name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct S2ExternalIds {
    #[serde(rename = "DOI")]
    doi: Option<String>,
    #[serde(rename = "ArXiv")]
    arxiv: Option<String>,
}

/// Best-effort DOI for an S2 paper: publisher DOI if present, else the
/// DataCite-registered arXiv DOI synthesized from `externalIds.ArXiv`.
/// arXiv DOIs in the `10.48550/arXiv.{id}` form resolve via doi.org and
/// are recognized by the downloader's arXiv fast-path.
fn resolve_doi(ids: Option<S2ExternalIds>) -> Option<String> {
    let ids = ids?;
    if let Some(doi) = ids.doi.filter(|s| !s.trim().is_empty()) {
        return Some(doi);
    }
    ids.arxiv
        .map(|id| id.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|id| format!("10.48550/arXiv.{id}"))
}

#[derive(Debug, Deserialize)]
struct S2Pdf {
    url: String,
}

/// Edge envelope for the SS Graph API `/references` and `/citations` endpoints.
/// `/references` populates `cited_paper`; `/citations` populates `citing_paper`.
#[derive(Debug, Deserialize)]
struct S2RefEdge {
    #[serde(rename = "citedPaper", default)]
    cited_paper: Option<S2Paper>,
    #[serde(rename = "citingPaper", default)]
    citing_paper: Option<S2Paper>,
}

#[derive(Debug, Deserialize)]
struct S2RefList {
    #[serde(default)]
    data: Vec<S2RefEdge>,
    #[serde(default)]
    next: Option<u32>,
    #[serde(default)]
    total: Option<u32>,
}

/// Single entry returned by `paper_references` / `paper_citations`. Stable
/// JSON shape for downstream MCP consumers (the `home-still-bridge` skill).
#[derive(Debug, Clone, Serialize)]
pub struct CitationGraphEntry {
    pub doi: Option<String>,
    pub title: String,
    pub year: Option<u16>,
    pub authors: Vec<String>,
    pub venue: Option<String>,
    pub citation_count: Option<u32>,
    pub semantic_scholar_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferencesResponse {
    pub references: Vec<CitationGraphEntry>,
    pub source: &'static str,
    pub truncated: bool,
    pub total_returned: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CitationsResponse {
    pub citations: Vec<CitationGraphEntry>,
    pub source: &'static str,
    pub total_available: Option<u32>,
    pub truncated: bool,
    pub total_returned: u32,
}

#[derive(Debug, Clone, Default)]
pub struct CitationsOpts {
    pub limit: Option<u32>,
    pub year_from: Option<u16>,
    pub sort: Option<String>,
}

fn s2_paper_to_entry(p: S2Paper) -> CitationGraphEntry {
    let semantic_scholar_id = p.paper_id.as_ref().filter(|s| !s.is_empty()).cloned();
    let authors: Vec<String> = p
        .authors
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            a.name.and_then(|n| {
                let trimmed = n.trim().to_string();
                (!trimmed.is_empty()).then_some(trimmed)
            })
        })
        .collect();
    let year = p.year.and_then(|y| u16::try_from(y).ok());
    let citation_count = p
        .citation_count
        .map(|c| u32::try_from(c).unwrap_or(u32::MAX));
    let doi = resolve_doi(p.external_ids);

    CitationGraphEntry {
        doi,
        title: p.title.unwrap_or_default(),
        year,
        authors,
        venue: p.venue,
        citation_count,
        semantic_scholar_id,
    }
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
        let doi = resolve_doi(s2.external_ids);

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
            id: s2.paper_id.unwrap_or_default(),
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

    /// Return the structured reference list of a paper by DOI.
    /// Single GET to `/graph/v1/paper/{ID}/references?limit=1000`, where
    /// `{ID}` is `DOI:{doi}` for normal DOIs and `ARXIV:{id}` for the
    /// arXiv DataCite form (SS doesn't index those under `DOI:`).
    pub async fn references(&self, doi: &str) -> Result<ReferencesResponse, PaperError> {
        let id = ss_identifier_for_doi(doi);
        let url = format!(
            "{}/graph/v1/paper/{}/references?fields=externalIds,title,year,authors,venue,citationCount&limit=1000",
            self.base_url, id
        );

        let mut request = self.client.get(&url);
        if let Some(ref key) = self.api_key {
            request = request.header("x-api-key", key);
        }

        let response = send_with_429_retry(request, "semantic_scholar").await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PaperError::NotFound(doi.to_string()));
        }
        check_response(&response, "semantic_scholar")?;

        let body: S2RefList = response.json().await.map_err(|e| {
            PaperError::ParseError(format!(
                "Failed to parse Semantic Scholar references: {}",
                e
            ))
        })?;

        let raw_count = body.data.len();
        let entries: Vec<CitationGraphEntry> = body
            .data
            .into_iter()
            .filter_map(|edge| edge.cited_paper)
            .map(s2_paper_to_entry)
            .collect();

        Ok(ReferencesResponse {
            total_returned: entries.len() as u32,
            references: entries,
            source: "semantic_scholar",
            // SS returns up to 1000 per page; if we filled the page assume more.
            truncated: raw_count >= 1000,
        })
    }

    /// Return the list of papers that cite a given DOI (forward chaining).
    /// Paginates SS's `/citations` endpoint at 1000 per page until `limit`
    /// reached or upstream exhausted; `year_from` filter and `sort` are
    /// applied post-fetch (SS does not filter citations reliably by year).
    pub async fn citations(
        &self,
        doi: &str,
        opts: CitationsOpts,
    ) -> Result<CitationsResponse, PaperError> {
        let id = ss_identifier_for_doi(doi);
        let effective_limit = opts.limit.unwrap_or(100).min(1000) as usize;

        const PAGE_SIZE: u32 = 1000;
        let mut entries: Vec<CitationGraphEntry> = Vec::new();
        let mut offset: u32 = 0;
        let mut total_available: Option<u32> = None;
        let mut hit_limit = false;

        loop {
            let url = format!(
                "{}/graph/v1/paper/{}/citations?fields=externalIds,title,year,authors,venue,citationCount&limit={}&offset={}",
                self.base_url, id, PAGE_SIZE, offset
            );

            let mut request = self.client.get(&url);
            if let Some(ref key) = self.api_key {
                request = request.header("x-api-key", key);
            }

            let response = send_with_429_retry(request, "semantic_scholar").await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(PaperError::NotFound(doi.to_string()));
            }
            check_response(&response, "semantic_scholar")?;

            let body: S2RefList = response.json().await.map_err(|e| {
                PaperError::ParseError(format!("Failed to parse Semantic Scholar citations: {}", e))
            })?;

            if total_available.is_none() {
                total_available = body.total;
            }

            let page_count = body.data.len();
            if page_count == 0 {
                break;
            }

            entries.extend(
                body.data
                    .into_iter()
                    .filter_map(|edge| edge.citing_paper)
                    .map(s2_paper_to_entry),
            );

            if entries.len() >= effective_limit {
                hit_limit = true;
                break;
            }

            if let Some(next_offset) = body.next {
                offset = next_offset;
            } else if page_count < PAGE_SIZE as usize {
                break;
            } else {
                offset += PAGE_SIZE;
            }
        }

        if let Some(yf) = opts.year_from {
            entries.retain(|e| e.year.map(|y| y >= yf).unwrap_or(false));
        }

        let sort_key = opts.sort.as_deref().unwrap_or("year");
        match sort_key {
            "citations" => entries.sort_by(|a, b| b.citation_count.cmp(&a.citation_count)),
            _ => entries.sort_by(|a, b| b.year.cmp(&a.year)),
        }

        if entries.len() > effective_limit {
            entries.truncate(effective_limit);
        }

        let total_returned = entries.len() as u32;
        let truncated = match total_available {
            Some(t) => t > total_returned,
            None => hit_limit,
        };

        Ok(CitationsResponse {
            citations: entries,
            source: "semantic_scholar",
            total_available,
            truncated,
            total_returned,
        })
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

        let response = send_with_429_retry(request, "semantic_scholar").await?;

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

        let response = send_with_429_retry(request, "semantic_scholar").await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> SemanticScholarProvider {
        SemanticScholarProvider::new(&SemanticScholarConfig::default())
            .expect("default config should build a provider")
    }

    #[test]
    fn search_response_round_trip_populates_doi_and_pdf() {
        // Captured-shape S2 bulk-search response: one paper has DOI + open-access
        // PDF; one has neither; one is arXiv-only (synthesizes DataCite DOI).
        let body = r#"{
            "total": 3,
            "data": [
                {
                    "paperId": "abc123",
                    "title": "Attention Is All You Need",
                    "abstract": "We propose a new simple network architecture.",
                    "year": 2017,
                    "authors": [{"name": "Ashish Vaswani"}],
                    "citationCount": 100000,
                    "externalIds": {"DOI": "10.48550/arXiv.1706.03762"},
                    "openAccessPdf": {"url": "https://arxiv.org/pdf/1706.03762.pdf"}
                },
                {
                    "paperId": "def456",
                    "title": "Some Closed-Access Paper",
                    "abstract": null,
                    "year": 2020,
                    "authors": [],
                    "citationCount": 5,
                    "externalIds": {},
                    "openAccessPdf": null
                },
                {
                    "paperId": "ghi789",
                    "title": "ArXiv-Only Preprint",
                    "abstract": "A paper that only exists on arXiv.",
                    "year": 2024,
                    "authors": [{"name": "Jane Researcher"}],
                    "citationCount": 3,
                    "externalIds": {"ArXiv": "2401.12345"},
                    "openAccessPdf": {"url": "https://arxiv.org/pdf/2401.12345.pdf"}
                }
            ]
        }"#;

        let parsed: S2SearchResponse = serde_json::from_str(body).expect("S2 fixture must parse");
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.data.len(), 3);

        let p = provider();
        let papers: Vec<Paper> = parsed
            .data
            .into_iter()
            .map(|s| p.s2_paper_to_paper(s))
            .collect();

        // First paper: DOI + PDF must survive the round trip.
        assert_eq!(papers[0].doi.as_deref(), Some("10.48550/arXiv.1706.03762"));
        assert_eq!(
            papers[0].download_urls,
            vec![String::from("https://arxiv.org/pdf/1706.03762.pdf")]
        );
        assert_eq!(papers[0].cited_by_count, Some(100000));

        // Second paper: empty externalIds + null openAccessPdf must yield None/empty,
        // not an error — this is the "coverage artifact" case.
        assert_eq!(papers[1].doi, None);
        assert!(papers[1].download_urls.is_empty());

        // Third paper: only ArXiv ID — synthesized DataCite DOI must be populated
        // so the downstream paper_download chain recognizes the 10.48550/arXiv prefix.
        assert_eq!(papers[2].doi.as_deref(), Some("10.48550/arXiv.2401.12345"));
    }

    #[test]
    fn resolve_doi_prefers_publisher_over_arxiv() {
        let ids = S2ExternalIds {
            doi: Some("10.1145/3528223.3530127".into()),
            arxiv: Some("2204.01234".into()),
        };
        assert_eq!(
            resolve_doi(Some(ids)).as_deref(),
            Some("10.1145/3528223.3530127")
        );
    }

    #[test]
    fn resolve_doi_synthesizes_when_only_arxiv() {
        let ids = S2ExternalIds {
            doi: None,
            arxiv: Some("hep-th/9711200".into()),
        };
        assert_eq!(
            resolve_doi(Some(ids)).as_deref(),
            Some("10.48550/arXiv.hep-th/9711200")
        );
    }

    #[test]
    fn resolve_doi_returns_none_when_empty() {
        assert_eq!(resolve_doi(Some(S2ExternalIds::default())), None);
        assert_eq!(resolve_doi(None), None);

        // Whitespace-only DOI / ArXiv values are treated as absent.
        let ids = S2ExternalIds {
            doi: Some("   ".into()),
            arxiv: Some("   ".into()),
        };
        assert_eq!(resolve_doi(Some(ids)), None);
    }

    // Captured shape of the SS Graph API `/paper/DOI:.../references` response.
    // One entry has a publisher DOI, one is arXiv-only (DataCite synthesis),
    // one has a null `externalIds` (the SS "thin record" case).
    const REFERENCES_FIXTURE: &str = r#"{
        "offset": 0,
        "data": [
            {
                "citedPaper": {
                    "paperId": "ref1",
                    "title": "BERT: Pre-training of Deep Bidirectional Transformers",
                    "year": 2018,
                    "authors": [{"name": "Jacob Devlin"}, {"name": "Ming-Wei Chang"}],
                    "venue": "NAACL",
                    "citationCount": 70000,
                    "externalIds": {"DOI": "10.18653/v1/N19-1423"}
                }
            },
            {
                "citedPaper": {
                    "paperId": "ref2",
                    "title": "Improving Language Understanding by Generative Pre-training",
                    "year": 2018,
                    "authors": [{"name": "Alec Radford"}],
                    "citationCount": 5000,
                    "externalIds": {"ArXiv": "1801.06146"}
                }
            },
            {
                "citedPaper": {
                    "paperId": "ref3",
                    "title": "Untracked Reference",
                    "year": null,
                    "authors": [],
                    "externalIds": null
                }
            }
        ]
    }"#;

    #[test]
    fn references_envelope_round_trip_to_entries() {
        let parsed: S2RefList =
            serde_json::from_str(REFERENCES_FIXTURE).expect("references fixture must parse");
        assert_eq!(parsed.data.len(), 3);

        let entries: Vec<CitationGraphEntry> = parsed
            .data
            .into_iter()
            .filter_map(|edge| edge.cited_paper)
            .map(s2_paper_to_entry)
            .collect();

        assert_eq!(entries.len(), 3);

        // Publisher DOI surfaces unchanged.
        assert_eq!(entries[0].doi.as_deref(), Some("10.18653/v1/N19-1423"));
        assert_eq!(entries[0].year, Some(2018));
        assert_eq!(entries[0].venue.as_deref(), Some("NAACL"));
        assert_eq!(entries[0].citation_count, Some(70000));
        assert_eq!(
            entries[0].authors,
            vec!["Jacob Devlin".to_string(), "Ming-Wei Chang".to_string()]
        );
        assert_eq!(entries[0].semantic_scholar_id.as_deref(), Some("ref1"));

        // arXiv-only entry: DataCite DOI synthesized from `ArXiv` external id.
        assert_eq!(entries[1].doi.as_deref(), Some("10.48550/arXiv.1801.06146"));
        assert_eq!(entries[1].venue, None);

        // Thin record with null externalIds: doi=None but the rest still
        // round-trips so the entry remains useful for downstream snowballing.
        assert_eq!(entries[2].doi, None);
        assert_eq!(entries[2].title, "Untracked Reference");
        assert!(entries[2].authors.is_empty());
        assert_eq!(entries[2].year, None);
    }

    // Captured shape of `/paper/DOI:.../citations` — note `total` and `next`
    // pagination markers, which are only present on this endpoint.
    const CITATIONS_FIXTURE: &str = r#"{
        "offset": 0,
        "next": 1000,
        "total": 137000,
        "data": [
            {
                "citingPaper": {
                    "paperId": "cite1",
                    "title": "Improving Transformers with Probabilistic Attention",
                    "year": 2022,
                    "authors": [{"name": "Some Researcher"}],
                    "venue": "ICML",
                    "citationCount": 42,
                    "externalIds": {"DOI": "10.1234/example.2022.001"}
                }
            },
            {
                "citingPaper": {
                    "paperId": "cite2",
                    "title": "Survey of Transformer Variants",
                    "year": 2024,
                    "authors": [{"name": "  "}, {"name": "Real Author"}],
                    "citationCount": 7,
                    "externalIds": {"ArXiv": "2401.99999"}
                }
            }
        ]
    }"#;

    #[test]
    fn citations_envelope_parses_pagination_metadata_and_filters_blank_authors() {
        let parsed: S2RefList =
            serde_json::from_str(CITATIONS_FIXTURE).expect("citations fixture must parse");

        // Pagination markers SS uses to tell us "there's more".
        assert_eq!(parsed.total, Some(137000));
        assert_eq!(parsed.next, Some(1000));
        assert_eq!(parsed.data.len(), 2);

        let entries: Vec<CitationGraphEntry> = parsed
            .data
            .into_iter()
            .filter_map(|edge| edge.citing_paper)
            .map(s2_paper_to_entry)
            .collect();

        assert_eq!(entries[0].doi.as_deref(), Some("10.1234/example.2022.001"));
        assert_eq!(entries[0].venue.as_deref(), Some("ICML"));

        // Whitespace-only author names are dropped; real names survive.
        assert_eq!(entries[1].authors, vec!["Real Author".to_string()]);
        assert_eq!(entries[1].doi.as_deref(), Some("10.48550/arXiv.2401.99999"));
    }

    #[test]
    fn ss_identifier_routes_arxiv_dois_to_arxiv_prefix() {
        // Pipeline-synthesized arXiv DataCite DOI → SS's ARXIV: route.
        // SS doesn't index this DOI form under DOI:, so without rerouting
        // every arXiv-only paper would 404 against the citation graph.
        assert_eq!(
            ss_identifier_for_doi("10.48550/arXiv.1706.03762"),
            "ARXIV:1706.03762"
        );
        // Casing of "arXiv" varies across the corpus; case-insensitive match.
        assert_eq!(
            ss_identifier_for_doi("10.48550/arxiv.2401.12345"),
            "ARXIV:2401.12345"
        );
        // Real publisher DOIs go through DOI:.
        assert_eq!(
            ss_identifier_for_doi("10.1145/3528223.3530127"),
            "DOI:10.1145/3528223.3530127"
        );
        // doi.org URL form is stripped before identifier selection.
        assert_eq!(
            ss_identifier_for_doi("https://doi.org/10.1145/3528223.3530127"),
            "DOI:10.1145/3528223.3530127"
        );
    }

    #[test]
    fn s2_paper_to_entry_handles_missing_paper_id_and_huge_citation_count() {
        // Empty/null paperId → semantic_scholar_id is None (we don't surface "").
        let p = S2Paper {
            paper_id: Some(String::new()),
            title: Some("Edge Case".into()),
            abstract_text: None,
            year: Some(2030),
            authors: None,
            citation_count: Some(u64::MAX),
            external_ids: None,
            open_access_pdf: None,
            venue: None,
        };
        let entry = s2_paper_to_entry(p);
        assert_eq!(entry.semantic_scholar_id, None);
        // u64::MAX casts to u32::MAX (saturating).
        assert_eq!(entry.citation_count, Some(u32::MAX));
        assert_eq!(entry.year, Some(2030));
    }

    // ── HTTP-level error mapping (wiremock) ─────────────────────────────
    //
    // These exercise the branches inside `references` / `citations` that
    // depend on the upstream HTTP status: 404 → NotFound, 5xx →
    // ProviderUnavailable, 429-then-200 → success via send_with_429_retry.
    // The 429 test takes ≈2s due to the retry helper's hard-coded backoff.

    fn provider_pointing_at(server_uri: &str) -> SemanticScholarProvider {
        let config = SemanticScholarConfig {
            base_url: server_uri.to_string(),
            ..SemanticScholarConfig::default()
        };
        SemanticScholarProvider::new(&config).expect("provider builds")
    }

    #[tokio::test]
    async fn references_404_maps_to_not_found() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/missing/references"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let err = provider
            .references("10.1234/missing")
            .await
            .expect_err("404 must surface as Err");
        assert!(
            matches!(err, PaperError::NotFound(ref d) if d == "10.1234/missing"),
            "expected NotFound(\"10.1234/missing\"), got {err:?}"
        );
    }

    #[tokio::test]
    async fn references_5xx_maps_to_provider_unavailable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/down/references"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let err = provider
            .references("10.1234/down")
            .await
            .expect_err("503 must surface as Err");
        assert!(
            matches!(err, PaperError::ProviderUnavailable(_)),
            "expected ProviderUnavailable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn references_retries_429_then_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // First call: 429. send_with_429_retry will sleep ~2s and retry.
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/retry/references"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Subsequent calls: real envelope.
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/retry/references"))
            .respond_with(ResponseTemplate::new(200).set_body_string(REFERENCES_FIXTURE))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let resp = provider
            .references("10.1234/retry")
            .await
            .expect("retry then success");
        assert_eq!(resp.source, "semantic_scholar");
        assert_eq!(resp.references.len(), 3);
    }

    #[tokio::test]
    async fn citations_404_maps_to_not_found() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/gone/citations"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let err = provider
            .citations("10.1234/gone", CitationsOpts::default())
            .await
            .expect_err("404 must surface as Err");
        assert!(matches!(err, PaperError::NotFound(_)), "got {err:?}");
    }

    // ── Live integration test ───────────────────────────────────────────
    //
    // Hits api.semanticscholar.org. Gated with #[ignore] because the repo
    // has no live-test feature flag — opt in with:
    //
    //     cargo test -p paper -- --ignored citation_graph_live
    //
    // Test DOI: 10.48550/arXiv.1706.03762 (Attention Is All You Need).

    #[tokio::test]
    #[ignore = "live API — run with `cargo test -p paper -- --ignored citation_graph_live`"]
    async fn citation_graph_live_attention_is_all_you_need() {
        let provider = SemanticScholarProvider::new(&SemanticScholarConfig::default())
            .expect("default config builds provider");

        let refs = provider
            .references("10.48550/arXiv.1706.03762")
            .await
            .expect("references call must succeed against live SS");
        assert!(
            refs.references.len() >= 30,
            "expected ≥30 references, got {}",
            refs.references.len()
        );
        assert_eq!(refs.source, "semantic_scholar");
        eprintln!(
            "live references: returned={} truncated={}",
            refs.total_returned, refs.truncated
        );

        let cites = provider
            .citations(
                "10.48550/arXiv.1706.03762",
                CitationsOpts {
                    limit: Some(200),
                    ..CitationsOpts::default()
                },
            )
            .await
            .expect("citations call must succeed against live SS");
        assert!(
            cites.citations.len() >= 100,
            "expected ≥100 citations, got {}",
            cites.citations.len()
        );
        // SS's `/citations` endpoint does not surface a `total` field on the
        // response envelope (verified 2026-05); `total_available` is therefore
        // expected to be None for now. We prove "upstream has more than the
        // 200-entry limit" via the truncation flag plus the returned count
        // hitting the limit exactly — both impossible to satisfy unless
        // upstream had >200 citations.
        assert_eq!(
            cites.total_returned, 200,
            "with limit=200 and 1000s of upstream citations, must return exactly 200"
        );
        assert!(cites.truncated, "limit=200 against >>1000 must truncate");
        eprintln!(
            "live citations: returned={} total_available={:?} truncated={}",
            cites.total_returned, cites.total_available, cites.truncated
        );
    }
}
