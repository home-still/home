use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

use crate::config::SemanticScholarConfig;
use crate::error::PaperError;
use crate::models::{Author, Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::downloader::strip_arxiv_doi_prefix;
use crate::providers::response::{check_response, send_with_429_retry};
use crate::resilience::guard::Guard;

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

/// Largest `limit` Semantic Scholar's search endpoint accepts.
const S2_SEARCH_PAGE_MAX: usize = 100;

pub struct SemanticScholarProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    max_retry_after: Duration,
}

impl SemanticScholarProvider {
    pub fn new(config: &SemanticScholarConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            max_retry_after: Duration::from_secs(config.max_retry_after_secs),
        })
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    /// `{base}/graph/v1/paper/{id}/{tail…}?{query}` with every piece
    /// percent-encoded. `id` is `DOI:<doi>` / `ARXIV:<id>`; a DOI's `/`
    /// separates path segments (Semantic Scholar takes them literally), and
    /// a `?`, `#` or space inside a DOI is encoded rather than ending the
    /// path.
    fn paper_url(
        &self,
        id: &str,
        tail: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Url, PaperError> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| PaperError::InvalidInput(format!("bad Semantic Scholar base_url: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| {
                PaperError::InvalidInput("Semantic Scholar base_url cannot be a base".into())
            })?
            .pop_if_empty()
            .extend(["graph", "v1", "paper"])
            .extend(id.split('/'))
            .extend(tail.iter().copied());
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Ok(url)
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
        let limit = query.max_results.min(S2_SEARCH_PAGE_MAX);
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
                // `before` is the first excluded day; S2's year range is inclusive.
                let last_included = before - chrono::Duration::days(1);
                range.push_str(&last_included.format("%Y").to_string());
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
        let id = ss_identifier_for_doi(&crate::stem::normalize_doi(doi)?);
        let url = self.paper_url(
            &id,
            &["references"],
            &[
                (
                    "fields",
                    "externalIds,title,year,authors,venue,citationCount",
                ),
                ("limit", "1000"),
            ],
        )?;

        let response = send_with_429_retry(
            self.authorized(self.client.get(url)),
            "semantic_scholar",
            self.max_retry_after,
        )
        .await?;

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
    /// Paginates SS's `/citations` endpoint at 1000 per page; `year_from`
    /// filter and `sort` are applied post-fetch (SS does not filter citations
    /// reliably by year). For `sort="citations"` the full citing set is
    /// fetched (bounded by `MAX_CITATION_SORT_FETCH`) so the ranking is global;
    /// for the default year sort, fetch stops once `limit` edges are gathered.
    ///
    /// Every page request runs under `guard` — the provider's shared
    /// limiter, breaker and retry — so a ten-page fetch is ten rate-limited,
    /// breaker-counted requests, not one.
    pub async fn citations(
        &self,
        doi: &str,
        opts: CitationsOpts,
        guard: &Guard,
    ) -> Result<CitationsResponse, PaperError> {
        let id = ss_identifier_for_doi(&crate::stem::normalize_doi(doi)?);
        let effective_limit = opts.limit.unwrap_or(100).min(1000) as usize;

        const PAGE_SIZE: u32 = 1000;
        // When sorting by citation count we must rank the global citing set,
        // not whatever the first `effective_limit` edges happened to be in SS's
        // default order — so fetch to completion, bounded by a sane cap.
        const MAX_CITATION_SORT_FETCH: usize = 10_000;
        // Semantic Scholar's citations endpoint rejects a request once
        // `offset + limit >= 10000` (verified empirically: offset 8999 + limit
        // 1000 = 9999 succeeds; offset 9000 = 10000 returns 400). Because
        // null-`citingPaper` edges are filtered out, `entries` grows slower than
        // `offset`, so the entry-count cap alone won't stop us in time — guard
        // the offset explicitly. For sort=citations this means we rank the top-N
        // among the first ~9000 edges SS will serve — its hard ceiling.
        const SS_CITATIONS_OFFSET_LIMIT: u32 = 10_000;
        // Backstop for the loop below: with the offset ceiling above, ten
        // 1000-edge pages is all Semantic Scholar will ever serve, so more
        // than this many requests means the cursor is not progressing.
        const MAX_CITATION_PAGES: u32 = 12;
        let sort_by_citations = opts.sort.as_deref() == Some("citations");
        let fetch_target = if sort_by_citations {
            MAX_CITATION_SORT_FETCH
        } else {
            effective_limit
        };
        let mut entries: Vec<CitationGraphEntry> = Vec::new();
        let mut offset: u32 = 0;
        let mut total_available: Option<u32> = None;
        let mut hit_limit = false;
        let mut pages: u32 = 0;

        loop {
            if offset.saturating_add(PAGE_SIZE) >= SS_CITATIONS_OFFSET_LIMIT {
                // Next page would hit SS's offset+limit ceiling — stop here
                // rather than issue a request SS rejects with 400.
                hit_limit = true;
                break;
            }
            pages += 1;
            if pages > MAX_CITATION_PAGES {
                return Err(PaperError::ParseError(format!(
                    "Semantic Scholar citations for {doi} needed more than {MAX_CITATION_PAGES} pages"
                )));
            }
            let body = guard
                .run(|| self.citations_page(&id, doi, offset, PAGE_SIZE))
                .await?;

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

            if entries.len() >= fetch_target {
                hit_limit = true;
                break;
            }

            if let Some(next_offset) = body.next {
                // `next` is remote data. A cursor that does not move forward
                // would re-request the same page forever (entries that are all
                // null edges never reach `fetch_target`), so it is an error.
                if next_offset <= offset {
                    return Err(PaperError::ParseError(format!(
                        "Semantic Scholar citations pagination did not advance for {doi} (offset {offset}, next {next_offset})"
                    )));
                }
                offset = next_offset;
            } else if page_count < PAGE_SIZE as usize {
                break;
            } else {
                offset = offset.saturating_add(PAGE_SIZE);
            }
        }

        if let Some(yf) = opts.year_from {
            entries.retain(|e| e.year.map(|y| y >= yf).unwrap_or(false));
        }

        let sort_key = opts.sort.as_deref().unwrap_or("year");
        match sort_key {
            "citations" => entries.sort_by_key(|e| std::cmp::Reverse(e.citation_count)),
            _ => entries.sort_by_key(|e| std::cmp::Reverse(e.year)),
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

    /// One `/citations` page: a single guarded unit (limiter, breaker and
    /// retry apply per page).
    async fn citations_page(
        &self,
        id: &str,
        doi: &str,
        offset: u32,
        page_size: u32,
    ) -> Result<S2RefList, PaperError> {
        let url = self.paper_url(
            id,
            &["citations"],
            &[
                (
                    "fields",
                    "externalIds,title,year,authors,venue,citationCount",
                ),
                ("limit", &page_size.to_string()),
                ("offset", &offset.to_string()),
            ],
        )?;

        let response = send_with_429_retry(
            self.authorized(self.client.get(url)),
            "semantic_scholar",
            self.max_retry_after,
        )
        .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PaperError::NotFound(doi.to_string()));
        }
        check_response(&response, "semantic_scholar")?;

        response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Semantic Scholar citations: {}", e))
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

        let response = send_with_429_retry(
            self.authorized(self.client.get(&url)),
            "semantic_scholar",
            self.max_retry_after,
        )
        .await?;

        check_response(&response, "semantic_scholar")?;

        let body: S2SearchResponse = response.json().await.map_err(|e| {
            PaperError::ParseError(format!("Failed to parse Semantic Scholar response: {}", e))
        })?;

        let papers: Vec<Paper> = body
            .data
            .into_iter()
            .map(|p| self.s2_paper_to_paper(p))
            .collect();

        let next_offset = query.offset + query.max_results.min(S2_SEARCH_PAGE_MAX);
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
            provider_failures: Vec::new(),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let id = ss_identifier_for_doi(&crate::stem::normalize_doi(doi)?);
        let url = self.paper_url(
            &id,
            &[],
            &[(
                "fields",
                "title,abstract,externalIds,openAccessPdf,year,authors,citationCount",
            )],
        )?;

        let response = send_with_429_retry(
            self.authorized(self.client.get(url)),
            "semantic_scholar",
            self.max_retry_after,
        )
        .await?;

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

    /// A guard whose limiter spacing is negligible, so paging tests stay fast.
    fn test_guard() -> Guard {
        Guard::new(
            "semantic_scholar",
            Duration::from_millis(1),
            &crate::resilience::config::ResilienceConfig::default(),
        )
        .expect("guard builds")
    }

    /// A guard at the configured production spacing, for the live tests.
    fn live_guard() -> Guard {
        Guard::new(
            "semantic_scholar",
            Duration::from_millis(SemanticScholarConfig::default().rate_limit_interval_ms),
            &crate::resilience::config::ResilienceConfig::default(),
        )
        .expect("guard builds")
    }

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
            .citations("10.1234/gone", CitationsOpts::default(), &test_guard())
            .await
            .expect_err("404 must surface as Err");
        assert!(matches!(err, PaperError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn citations_sort_by_citations_ranks_globally_across_pages() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Page 1 (offset=0): a full page of low-citation edges. The single
        // most-cited edge lives on PAGE 2 — so an early-break-on-limit fetch
        // would never see it and would rank a citationCount=1 edge first.
        let mut page1_data = String::from("[");
        for i in 0..1000 {
            if i > 0 {
                page1_data.push(',');
            }
            page1_data.push_str(&format!(
                r#"{{"citingPaper":{{"paperId":"low{i}","title":"Low cite {i}","year":2020,"citationCount":1,"externalIds":{{"DOI":"10.1/low{i}"}}}}}}"#
            ));
        }
        page1_data.push(']');
        let page1 = format!(r#"{{"offset":0,"next":1000,"total":1001,"data":{page1_data}}}"#);
        let page2 = r#"{"offset":1000,"total":1001,"data":[{"citingPaper":{"paperId":"top","title":"Most cited","year":2021,"citationCount":9999,"externalIds":{"DOI":"10.1/top"}}}]}"#;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/popular/citations"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page1))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/popular/citations"))
            .and(query_param("offset", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page2))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let resp = provider
            .citations(
                "10.1234/popular",
                CitationsOpts {
                    limit: Some(100),
                    sort: Some("citations".to_string()),
                    ..CitationsOpts::default()
                },
                &test_guard(),
            )
            .await
            .expect("citations must succeed");

        // The globally most-cited edge (page 2) must rank first, proving the
        // fetch paginated past the first page before sorting.
        assert_eq!(
            resp.citations.first().and_then(|c| c.citation_count),
            Some(9999)
        );
        assert_eq!(
            resp.citations.first().map(|c| c.title.as_str()),
            Some("Most cited")
        );
    }

    #[tokio::test]
    async fn citations_sort_stops_at_ss_offset_ceiling_without_400() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Pages at offsets 0..=8000. Each returns 1000 edges but only 50 carry a
        // citingPaper (the rest are null and get filtered), so `entries` lags
        // `offset` — the real condition that walked the old code past Semantic
        // Scholar's `offset+limit < 10000` ceiling into a 400. We deliberately do
        // NOT mount offset=9000 (the first offset SS rejects): if the guard is
        // too loose and the loop requests it, the unmatched mock 404s and the
        // call errors, failing this test.
        let server = MockServer::start().await;
        for page in 0..=8u32 {
            let offset = page * 1000;
            let mut data = String::from("[");
            for i in 0..1000u32 {
                if i > 0 {
                    data.push(',');
                }
                if i < 50 {
                    let cc = offset + i;
                    data.push_str(&format!(
                        r#"{{"citingPaper":{{"paperId":"p{offset}_{i}","title":"cite {offset}_{i}","year":2020,"citationCount":{cc},"externalIds":{{"DOI":"10.1/{offset}_{i}"}}}}}}"#
                    ));
                } else {
                    data.push_str(r#"{"citingPaper":null}"#);
                }
            }
            data.push(']');
            let next = offset + 1000;
            let body =
                format!(r#"{{"offset":{offset},"next":{next},"total":50000,"data":{data}}}"#);
            Mock::given(method("GET"))
                .and(path("/graph/v1/paper/DOI:10.1234/huge/citations"))
                .and(query_param("offset", offset.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }

        let provider = provider_pointing_at(&server.uri());
        let resp = provider
            .citations(
                "10.1234/huge",
                CitationsOpts {
                    limit: Some(10),
                    sort: Some("citations".to_string()),
                    ..CitationsOpts::default()
                },
                &test_guard(),
            )
            .await
            .expect("must stop at SS offset ceiling, not request past it and 400");

        // Got results (didn't error), ranked by citation_count. The highest
        // planted count is on the last fetched page (offset 8000 + 49).
        assert_eq!(resp.citations.len(), 10);
        assert_eq!(
            resp.citations.first().and_then(|c| c.citation_count),
            Some(8049)
        );
    }

    #[tokio::test]
    #[ignore = "live API — run with `cargo test -p paper -- --ignored citations_sort_live`"]
    async fn citations_sort_live_high_citation_paper_no_400() {
        // BERT (~80k citations) — exercises the deep-pagination path against the
        // real SS offset ceiling. Regression guard for rc.343/rc.344: must not
        // 400 past offset 8999, and results must be citation-count descending.
        let provider = SemanticScholarProvider::new(&SemanticScholarConfig::default())
            .expect("default config builds provider");
        let resp = provider
            .citations(
                "10.18653/v1/N19-1423",
                CitationsOpts {
                    limit: Some(5),
                    sort: Some("citations".to_string()),
                    ..CitationsOpts::default()
                },
                &live_guard(),
            )
            .await
            .expect("sort=citations on a high-citation paper must not 400");
        assert!(!resp.citations.is_empty(), "expected citing papers");
        let counts: Vec<u32> = resp
            .citations
            .iter()
            .map(|c| c.citation_count.unwrap_or(0))
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] >= w[1]),
            "citations must be sorted descending by count: {counts:?}"
        );
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
                &live_guard(),
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

    fn citations_page(next: Option<u32>, with_paper: bool) -> String {
        let edge = if with_paper {
            r#"{"citingPaper":{"paperId":"p1","title":"T","year":2020,"externalIds":{"DOI":"10.1/x"}}}"#
        } else {
            r#"{"citingPaper":null}"#
        };
        let next = next.map_or(String::new(), |n| format!(r#","next":{n}"#));
        format!(r#"{{"total":5000,"data":[{edge}]{next}}}"#)
    }

    #[tokio::test]
    async fn a_citations_cursor_that_does_not_advance_is_an_error_not_a_hang() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Only null edges, `next` stuck at 0: `entries` never grows and
        // `offset` never moves, so the old loop never ended.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(citations_page(Some(0), false)),
            )
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.citations("10.1/x", CitationsOpts::default(), &test_guard()),
        )
        .await
        .expect("pagination must terminate");

        let err = result.expect_err("a stuck cursor must be an error");
        assert!(
            matches!(&err, PaperError::ParseError(m) if m.contains("did not advance")),
            "{err:?}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A three-page citing set, one edge per page, chained by `next`.
    async fn mount_three_citation_pages(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        for (offset, next) in [(0u32, Some(1000u32)), (1000, Some(2000)), (2000, None)] {
            Mock::given(method("GET"))
                .and(path("/graph/v1/paper/DOI:10.1/pages/citations"))
                .and(query_param("offset", offset.to_string()))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(citations_page(next, true)),
                )
                .mount(server)
                .await;
        }
    }

    #[tokio::test]
    async fn every_citations_page_waits_for_the_rate_limiter() {
        // Three pages through a limiter spaced 300 ms apart: the first request
        // is immediate, each of the other two waits a full interval. Taking the
        // limiter once per call (the old behaviour) finishes in a few ms.
        let server = wiremock::MockServer::start().await;
        mount_three_citation_pages(&server).await;
        let interval = Duration::from_millis(300);
        let guard = Guard::new(
            "semantic_scholar",
            interval,
            &crate::resilience::config::ResilienceConfig::default(),
        )
        .unwrap();

        let provider = provider_pointing_at(&server.uri());
        let started = std::time::Instant::now();
        provider
            .citations(
                "10.1/pages",
                CitationsOpts {
                    limit: Some(10),
                    sort: Some("citations".to_string()),
                    ..CitationsOpts::default()
                },
                &guard,
            )
            .await
            .expect("three pages");

        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert!(
            started.elapsed() >= interval * 2,
            "3 page requests must be spaced by the limiter, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_failing_citations_page_is_retried_alone() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        // Page 2 fails once with a 503. The retry re-requests page 2 only —
        // page 1 is fetched exactly once — so each page is its own guarded unit.
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1/pages/citations"))
            .and(query_param("offset", "1000"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_three_citation_pages(&server).await;
        let config = crate::resilience::config::ResilienceConfig {
            retry_max_attempts: 2,
            retry_min_backoff_ms: 1,
            retry_max_backoff_secs: 1,
            ..crate::resilience::config::ResilienceConfig::default()
        };
        let guard = Guard::new("semantic_scholar", Duration::from_millis(1), &config).unwrap();

        let provider = provider_pointing_at(&server.uri());
        provider
            .citations(
                "10.1/pages",
                CitationsOpts {
                    limit: Some(10),
                    sort: Some("citations".to_string()),
                    ..CitationsOpts::default()
                },
                &guard,
            )
            .await
            .expect("page 2 recovers on retry");

        let offsets: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter_map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "offset")
                    .map(|(_, v)| v.into_owned())
            })
            .collect();
        assert_eq!(offsets, ["0", "1000", "1000", "2000"]);
    }

    #[tokio::test]
    async fn a_huge_next_offset_stops_paging_without_overflowing() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // `offset + PAGE_SIZE` overflowed u32 (panic in debug, wrap in release).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(citations_page(Some(u32::MAX), true)),
            )
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        let resp = provider
            .citations(
                "10.1/x",
                CitationsOpts {
                    limit: Some(500),
                    ..CitationsOpts::default()
                },
                &test_guard(),
            )
            .await
            .expect("stops at the offset ceiling");
        assert_eq!(resp.citations.len(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reserved_characters_in_a_doi_are_encoded_not_interpreted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/graph/v1/paper/DOI:10.1234/a%3Fb%23c/references"))
            .respond_with(ResponseTemplate::new(200).set_body_string(REFERENCES_FIXTURE))
            .mount(&server)
            .await;

        let provider = provider_pointing_at(&server.uri());
        provider
            .references("10.1234/a?b#c")
            .await
            .expect("`?` and `#` stay inside the path segment");
    }

    #[test]
    fn malformed_dois_are_invalid_input_before_any_request() {
        let provider = provider_pointing_at("http://127.0.0.1:1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for doi in ["", "nope", "10.1234/../x"] {
            let err = rt
                .block_on(provider.references(doi))
                .expect_err("must be rejected");
            assert!(
                matches!(err, PaperError::InvalidInput(_)),
                "{doi:?}: {err:?}"
            );
        }
    }

    #[test]
    fn year_range_is_inclusive_of_the_last_day_included() {
        // `<=2019` is `before = 2020-01-01` (exclusive); S2's `year=-2020`
        // would return 2020 papers.
        let provider = provider();
        let query = SearchQuery {
            query: "x".to_string(),
            search_type: SearchType::Keywords,
            max_results: 250,
            offset: 0,
            date_filter: Some(crate::models::DateFilter::parse(">=2015 <=2019").unwrap()),
            sort_by: SortBy::default(),
            min_citations: None,
        };
        let url = Url::parse(&provider.build_search_url(&query).unwrap()).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(param("year").as_deref(), Some("2015-2019"));
        assert_eq!(param("limit").as_deref(), Some("100"));
    }
}
