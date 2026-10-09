use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use roxmltree;

use crate::config::ArxivConfig;
use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType, SortBy};
use crate::ports::provider::PaperProvider;
use crate::providers::response::{retry_after, status_error};

pub struct ArxivProvider {
    client: Client,
    base_url: String,
}

/// Parse the leading `YYYY-MM-DD` of an Atom `<published>` timestamp.
///
/// The timestamp is remote text: `str::get` keeps a multi-byte character
/// inside the first ten bytes from panicking the way `&s[..10]` did. Anything
/// shorter than a date, or not a calendar date, yields `None` — the date is
/// optional metadata, and dropping a malformed one must not discard the
/// whole entry.
fn parse_published_date(s: &str) -> Option<chrono::NaiveDate> {
    let date = s.get(..10)?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Boolean operators arXiv's query language understands. They stay operators;
/// only the terms between them get a field prefix.
const OPERATORS: [&str; 4] = ["AND", "OR", "NOT", "ANDNOT"];

/// `text` as an arXiv search expression over the field named by `prefix`
/// (`all:`, `ti:`, `au:`).
///
/// Plain multi-word text is one quoted phrase. Otherwise every term gets the
/// prefix, operators pass through untouched (prefixing one — `all:AND` — made
/// arXiv reject the whole request), a `"quoted phrase"` stays one term, and
/// two terms with no operator between them are joined by `AND`.
fn field_expression(prefix: &str, text: &str) -> Result<String, PaperError> {
    if super::query_utils::is_phrase_query(text) {
        return Ok(format!("{prefix}\"{text}\""));
    }
    let mut parts: Vec<String> = Vec::new();
    // The words of a quoted phrase that has been opened and not yet closed.
    let mut phrase: Vec<&str> = Vec::new();
    let mut previous_was_term = false;
    for word in text.split_whitespace() {
        let term = if phrase.is_empty() {
            if OPERATORS.contains(&word) {
                parts.push(word.to_string());
                previous_was_term = false;
                continue;
            }
            if word.starts_with('"') && !(word.len() > 1 && word.ends_with('"')) {
                phrase.push(word);
                continue;
            }
            word.to_string()
        } else {
            phrase.push(word);
            if !word.ends_with('"') {
                continue;
            }
            std::mem::take(&mut phrase).join(" ")
        };
        if previous_was_term {
            parts.push(String::from("AND"));
        }
        parts.push(format!("{prefix}{term}"));
        previous_was_term = true;
    }
    if !phrase.is_empty() {
        return Err(PaperError::InvalidInput(format!(
            "unbalanced double quote in search query {text:?}"
        )));
    }
    Ok(parts.join(" "))
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

    /// Fetch a paper by bare arXiv id (e.g. `"2005.11401"`, `"2312.10997"`).
    /// Uses the `id_list=` form of the arXiv API, which is more reliable than
    /// the text-search form when the id is known. Returns `Ok(None)` if no
    /// entry matches — callers that hit this via an arXiv-prefix DOI should
    /// treat `None` as "not found," not as an error.
    pub async fn get_by_arxiv_id(&self, id: &str) -> Result<Option<Paper>, PaperError> {
        let url = url::Url::parse_with_params(&self.base_url, &[("id_list", id)])
            .map_err(|e| PaperError::InvalidInput(e.to_string()))?;
        let xml = self.get_feed(url.as_str()).await?;
        let (papers, _) = self.parse_atom_feed(&xml)?;
        Ok(papers.into_iter().next())
    }

    /// GET `url` and return the Atom body. arXiv throttles aggressively (429,
    /// or 503 with a `Retry-After`); one bounded retry usually clears it, and
    /// a second refusal is a `RateLimited` carrying the server's directive.
    async fn get_feed(&self, url: &str) -> Result<String, PaperError> {
        let throttled = |status: reqwest::StatusCode| {
            matches!(
                status,
                reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
            )
        };
        let mut response = self.client.get(url).send().await?;
        if throttled(response.status()) {
            let wait = retry_after(response.headers())
                .unwrap_or(std::time::Duration::from_secs(2))
                .min(std::time::Duration::from_secs(5));
            tokio::time::sleep(wait).await;
            response = self.client.get(url).send().await?;
        }
        if throttled(response.status()) {
            return Err(PaperError::RateLimited {
                provider: String::from("arxiv"),
                retry_after: retry_after(response.headers()),
            });
        } else if !response.status().is_success() {
            return Err(status_error("arxiv", response.status()));
        }
        Ok(response.text().await?)
    }

    fn build_search_url(&self, query: &SearchQuery) -> Result<String, PaperError> {
        let search_prefix = match query.search_type {
            SearchType::Keywords => "all:",
            SearchType::Title => "ti:",
            SearchType::Author => "au:",
            _ => "all:",
        };

        let search_query = if matches!(query.search_type, SearchType::DOI) {
            // An identifier, not search text: one quoted phrase, whatever
            // punctuation (parentheses, colons) the DOI carries.
            format!("{search_prefix}\"{}\"", query.query.replace('"', " "))
        } else {
            field_expression(search_prefix, &query.query)?
        };
        let search_query = if let Some(ref df) = query.date_filter {
            let from = df
                .after
                .map(|d| format!("{}0000", d.format("%Y%m%d")))
                .unwrap_or_else(|| "000001010000".to_string());
            let to = df
                .before
                .map(|d| {
                    let day_before = d - chrono::Duration::days(1);
                    format!("{}2359", day_before.format("%Y%m%d"))
                })
                .unwrap_or_else(|| "999912312359".to_string());
            // Parenthesised: `a OR b AND submittedDate:[…]` filters only `b`.
            format!("({}) AND submittedDate:[{} TO {}]", search_query, from, to)
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
            .and_then(parse_published_date);

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

        let entries: Vec<roxmltree::Node> = root
            .children()
            .filter(|n| n.has_tag_name((ns, "entry")))
            .collect();

        // The arXiv API reports a bad request (malformed id, bad query) as a
        // 200 feed with ONE entry whose `<id>` is under `/api/errors` and
        // whose `<summary>` is the message. It is not a paper.
        if let Some(failure) = entries.iter().find(|entry| {
            entry
                .children()
                .find(|n| n.has_tag_name((ns, "id")))
                .and_then(|n| n.text())
                .is_some_and(|id| id.contains("arxiv.org/api/errors"))
        }) {
            let message = failure
                .children()
                .find(|n| n.has_tag_name((ns, "summary")))
                .and_then(|n| n.text())
                .unwrap_or("no message")
                .trim();
            return Err(PaperError::InvalidInput(format!(
                "arXiv rejected the request: {message}"
            )));
        }

        let papers: Vec<Paper> = entries
            .into_iter()
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
        let xml = self.get_feed(&url).await?;
        let (papers, total_results) = self.parse_atom_feed(&xml)?;

        Ok(SearchResult {
            papers,
            total_results,
            next_offset: Some(query.offset.saturating_add(query.max_results))
                .filter(|&n| n < total_results),
            provider: String::from("arxiv"),
            provider_failures: Vec::new(),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperError> {
        let doi = crate::stem::normalize_doi(doi)?;

        // DataCite-registered arXiv DOIs (`10.48550/arXiv.<id>`, any casing)
        // have a direct translation to an arXiv id. Use the id_list API
        // rather than relevance search — it's both faster and correct.
        if let Some(arxiv_id) = super::downloader::strip_arxiv_doi_prefix(&doi) {
            return self.get_by_arxiv_id(arxiv_id).await;
        }

        // A non-arXiv DOI is looked up as text (arXiv has no DOI index), so
        // the best text match is not necessarily that paper: only an entry
        // that names this very DOI is the answer, anything else is "none".
        let query = SearchQuery {
            query: doi.clone(),
            search_type: SearchType::DOI,
            max_results: 5,
            offset: 0,
            date_filter: None,
            sort_by: SortBy::default(),
            min_citations: None,
        };

        let result = self.search_by_query(&query).await?;
        Ok(result.papers.into_iter().find(|paper| {
            paper
                .doi
                .as_deref()
                .and_then(|d| crate::stem::normalize_doi(d).ok())
                .is_some_and(|d| d.eq_ignore_ascii_case(&doi))
        }))
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

    /// The decoded `search_query` parameter the provider would send.
    fn search_query_for(
        search_type: SearchType,
        text: &str,
        date_filter: Option<crate::models::DateFilter>,
    ) -> Result<String, PaperError> {
        let query = SearchQuery {
            query: text.to_string(),
            search_type,
            max_results: 10,
            offset: 0,
            date_filter,
            sort_by: SortBy::default(),
            min_citations: None,
        };
        let url = provider().build_search_url(&query)?;
        let url = url::Url::parse(&url).expect("builder emits a valid URL");
        let sent = url
            .query_pairs()
            .find(|(key, _)| key == "search_query")
            .map(|(_, value)| value.into_owned())
            .expect("search_query is always sent");
        Ok(sent)
    }

    #[test]
    fn boolean_operators_stay_operators_and_only_terms_get_the_field_prefix() {
        // `all:neural AND all:AND AND all:networks` made arXiv answer with an
        // error entry, so every boolean query failed on this provider.
        let sent = |text: &str| search_query_for(SearchType::Keywords, text, None).unwrap();
        assert_eq!(sent("neural AND networks"), "all:neural AND all:networks");
        assert_eq!(
            sent("cats OR dogs NOT mice"),
            "all:cats OR all:dogs NOT all:mice"
        );
        assert_eq!(
            search_query_for(SearchType::Title, "a AND b", None).unwrap(),
            "ti:a AND ti:b"
        );
    }

    #[test]
    fn plain_text_and_quoted_phrases_keep_their_meaning() {
        let sent = |text: &str| search_query_for(SearchType::Keywords, text, None).unwrap();
        assert_eq!(sent("CRISPR"), "all:CRISPR");
        assert_eq!(sent("deep learning"), "all:\"deep learning\"");
        assert_eq!(
            sent("\"deep learning\" attention"),
            "all:\"deep learning\" AND all:attention"
        );
        assert_eq!(sent("\"exact\" OR loose"), "all:\"exact\" OR all:loose");
    }

    #[test]
    fn an_unclosed_quote_is_invalid_input_not_a_malformed_request() {
        let err = search_query_for(SearchType::Keywords, "\"deep learning", None).unwrap_err();
        assert!(matches!(err, PaperError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn the_date_filter_binds_the_whole_expression_not_just_its_last_term() {
        // `a OR b AND submittedDate:[…]` applies the range to `b` alone.
        let filter = crate::models::DateFilter::parse(">=2024-01 <2024-02").unwrap();
        let sent = search_query_for(SearchType::Keywords, "cats OR dogs", Some(filter)).unwrap();
        assert_eq!(
            sent,
            "(all:cats OR all:dogs) AND submittedDate:[202401010000 TO 202401312359]"
        );
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

    #[test]
    fn published_date_parses_the_leading_calendar_date() {
        assert_eq!(
            parse_published_date("2023-01-15T00:00:00Z"),
            chrono::NaiveDate::from_ymd_opt(2023, 1, 15)
        );
        assert_eq!(
            parse_published_date("2023-01-15"),
            chrono::NaiveDate::from_ymd_opt(2023, 1, 15)
        );
    }

    #[test]
    fn published_date_never_panics_on_short_or_multibyte_text() {
        // `2023-01-1é…` and `日本語…` put a multi-byte char across byte 10:
        // `&s[..10]` aborted the process on each of them.
        for s in [
            "",
            "2023",
            "2023-01-1",
            "2023-01-1é",
            "2023-01-1é5T00:00:00Z",
            "20é3-01-15",
            "日本語の日付です",
        ] {
            assert_eq!(parse_published_date(s), None, "input: {s:?}");
        }
    }

    #[test]
    fn entry_with_a_hostile_published_field_still_yields_the_paper() {
        let p = provider();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
              <feed xmlns="http://www.w3.org/2005/Atom"
                    xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/">
                  <opensearch:totalResults>1</opensearch:totalResults>
                  <entry>
                      <id>http://arxiv.org/abs/2301.00001v1</id>
                      <title>Test Paper Title</title>
                      <published>2023-01-1é5T00:00:00Z</published>
                  </entry>
              </feed>"#;

        let (papers, _) = p.parse_atom_feed(xml).expect("feed parses");
        assert_eq!(papers.len(), 1);
        assert_eq!(papers[0].publication_date, None);
    }

    #[test]
    fn an_api_error_feed_is_an_error_not_a_paper() {
        // arXiv answers a malformed `id_list` with HTTP 200 and one entry
        // under `/api/errors`; it used to surface as a paper titled "Error".
        let p = provider();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
              <feed xmlns="http://www.w3.org/2005/Atom"
                    xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/">
                  <opensearch:totalResults>1</opensearch:totalResults>
                  <entry>
                      <id>http://arxiv.org/api/errors#incorrect_id_format_for_foo</id>
                      <title>Error</title>
                      <summary>incorrect id format for foo</summary>
                  </entry>
              </feed>"#;

        let err = p.parse_atom_feed(xml).expect_err("error feed must fail");
        assert!(
            matches!(&err, PaperError::InvalidInput(m) if m.contains("incorrect id format")),
            "{err:?}"
        );
    }

    fn doi_feed(entries: &[(&str, &str)]) -> String {
        let entries: String = entries
            .iter()
            .map(|(id, doi)| {
                format!(
                    r#"<entry><id>http://arxiv.org/abs/{id}</id><title>Paper {id}</title><arxiv:doi>{doi}</arxiv:doi></entry>"#
                )
            })
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
              <feed xmlns="http://www.w3.org/2005/Atom"
                    xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/"
                    xmlns:arxiv="http://arxiv.org/schemas/atom">
                  <opensearch:totalResults>2</opensearch:totalResults>{entries}
              </feed>"#
        )
    }

    #[tokio::test]
    async fn a_text_hit_for_a_non_arxiv_doi_is_the_paper_only_if_it_names_that_doi() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // arXiv has no DOI index: the DOI is searched as text, so the top hit
        // can be any paper that merely mentions it. The aggregate merged that
        // paper into the answer for the DOI.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_string(doi_feed(&[
                ("2001.00001v1", "10.1234/somebody-else"),
                ("2001.00002v1", "10.1234/Wanted"),
            ])))
            .mount(&server)
            .await;
        let config = ArxivConfig {
            base_url: format!("{}/api/query", server.uri()),
            ..ArxivConfig::default()
        };
        let arxiv = ArxivProvider::new(&config).unwrap();

        let found = arxiv.get_by_doi("10.1234/wanted").await.unwrap();
        assert_eq!(found.map(|p| p.id), Some("2001.00002v1".to_string()));

        let none = arxiv.get_by_doi("10.1234/unrelated").await.unwrap();
        assert!(none.is_none(), "{none:?}");
    }
}
