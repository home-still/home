//! `discover`: look outside the corpus for works worth adding to it.
//!
//! Two sources, never mixed in one call. `providers` asks the online paper
//! providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE).
//! `openalex` reads the local OpenAlex corpus, which answers without the
//! network and also knows a topic's most cited authors. A call either
//! searches (`query`) or looks one work up (`doi`); a combination that the
//! source cannot answer is refused, never answered with part of it dropped.

use duckdb::types::Value as Sql;
use rmcp::schemars;

use crate::openalex::opt_value;
use crate::{to_json, HomeStillMcp};

/// How many results a search returns when the caller names no `limit`.
pub const DISCOVER_DEFAULT_LIMIT: u32 = 10;
/// The most results one search returns.
pub const DISCOVER_MAX_LIMIT: u32 = 200;
/// How many topic names an "unknown topic" refusal offers.
const TOPIC_SUGGESTIONS: i64 = 10;

/// Where `discover` looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverSource {
    /// The online paper providers.
    Providers,
    /// The local OpenAlex corpus.
    Openalex,
}

/// What `discover` returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverKind {
    /// Works: papers, books, datasets.
    #[default]
    Works,
    /// The most cited authors of one OpenAlex topic.
    Authors,
}

/// The arguments of `discover`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiscoverParams {
    #[schemars(
        description = "Where to look: `providers` (the online paper providers) or `openalex` (the local OpenAlex corpus; no network)."
    )]
    pub source: DiscoverSource,
    #[schemars(
        description = "What to search for. Give this or `doi`, not both. With `kind: \"authors\"` it names an OpenAlex topic: its id (`T10001`) or its exact name."
    )]
    pub query: Option<String>,
    #[schemars(description = "The DOI of one work to look up. Give this or `query`, not both.")]
    pub doi: Option<String>,
    #[schemars(
        description = "`works` (the default) or `authors`: the most cited authors of the OpenAlex topic `query` names (`source: \"openalex\"` only)."
    )]
    #[serde(default)]
    pub kind: DiscoverKind,
    #[schemars(
        description = "Only works published in this year or later. Searches for works only."
    )]
    pub year_from: Option<u16>,
    #[schemars(
        description = "Only works published in this year or earlier. Searches for works only."
    )]
    pub year_to: Option<u16>,
    #[schemars(description = "How many results a search returns: 1 to 200, 10 by default.")]
    pub limit: Option<u32>,
}

/// One answerable request. Every combination of arguments is one of these
/// or a refusal.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    ProviderSearch {
        query: String,
        year_from: Option<u16>,
        year_to: Option<u16>,
        limit: u32,
    },
    ProviderWork {
        doi: String,
    },
    CorpusSearch {
        query: String,
        year_from: Option<u16>,
        year_to: Option<u16>,
        limit: u32,
    },
    CorpusWork {
        doi: String,
    },
    CorpusAuthors {
        topic: String,
        limit: u32,
    },
}

/// A text argument with its surrounding whitespace removed. One that is
/// given but blank is a mistake, not an absent argument.
fn given(value: Option<String>, name: &str) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Err(format!("`{name}` is empty")),
        Some(text) => Ok(Some(text.trim().to_string())),
    }
}

/// The number of results to return. A `limit` outside the range is refused:
/// lowering it silently would answer a smaller question than was asked.
fn result_limit(limit: Option<u32>) -> Result<u32, String> {
    match limit {
        None => Ok(DISCOVER_DEFAULT_LIMIT),
        Some(n) if (1..=DISCOVER_MAX_LIMIT).contains(&n) => Ok(n),
        Some(n) => Err(format!(
            "`limit` must be 1 to {DISCOVER_MAX_LIMIT}, got {n}"
        )),
    }
}

/// Decide what a call asks for, or why it cannot be answered.
fn plan(p: DiscoverParams) -> Result<Request, String> {
    let query = given(p.query, "query")?;
    let doi = given(p.doi, "doi")?;
    if let (Some(from), Some(to)) = (p.year_from, p.year_to) {
        if from > to {
            return Err(format!("`year_from` ({from}) is after `year_to` ({to})"));
        }
    }
    let has_years = p.year_from.is_some() || p.year_to.is_some();
    match (query, doi) {
        (Some(_), Some(_)) => Err(
            "give `query` or `doi`, not both: a query searches for many works, a DOI names one"
                .to_string(),
        ),
        (None, None) => Err("give `query` (to search) or `doi` (to look one work up)".to_string()),
        (None, Some(doi)) => {
            if p.kind == DiscoverKind::Authors {
                return Err(
                    "`kind: \"authors\"` takes a `query` naming an OpenAlex topic, not a `doi`"
                        .to_string(),
                );
            }
            if has_years || p.limit.is_some() {
                return Err(
                    "`year_from`, `year_to` and `limit` shape a search; a `doi` names one work, \
                     so leave them out"
                        .to_string(),
                );
            }
            Ok(match p.source {
                DiscoverSource::Providers => Request::ProviderWork { doi },
                DiscoverSource::Openalex => Request::CorpusWork { doi },
            })
        }
        (Some(query), None) => {
            let limit = result_limit(p.limit)?;
            match (p.kind, p.source) {
                (DiscoverKind::Authors, DiscoverSource::Providers) => Err(
                    "`kind: \"authors\"` is answered from the local OpenAlex corpus only: use \
                     `source: \"openalex\"`"
                        .to_string(),
                ),
                (DiscoverKind::Authors, DiscoverSource::Openalex) if has_years => Err(
                    "`year_from` and `year_to` filter works; `kind: \"authors\"` ranks a topic's \
                     authors over all years"
                        .to_string(),
                ),
                (DiscoverKind::Authors, DiscoverSource::Openalex) => Ok(Request::CorpusAuthors {
                    topic: query,
                    limit,
                }),
                (DiscoverKind::Works, DiscoverSource::Providers) => Ok(Request::ProviderSearch {
                    query,
                    year_from: p.year_from,
                    year_to: p.year_to,
                    limit,
                }),
                (DiscoverKind::Works, DiscoverSource::Openalex) => Ok(Request::CorpusSearch {
                    query,
                    year_from: p.year_from,
                    year_to: p.year_to,
                    limit,
                }),
            }
        }
    }
}

/// The providers' date filter for a year range.
fn year_filter(
    year_from: Option<u16>,
    year_to: Option<u16>,
) -> Result<Option<paper::models::DateFilter>, String> {
    let bounds: Vec<String> = [
        year_from.map(|year| format!(">={year}")),
        year_to.map(|year| format!("<={year}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if bounds.is_empty() {
        return Ok(None);
    }
    paper::models::DateFilter::parse(&bounds.join(" "))
        .map(Some)
        .map_err(|e| format!("invalid year filter: {e}"))
}

/// The DOI as the local OpenAlex corpus stores it. An OpenAlex id is not a
/// DOI, and is refused rather than looked up under another column.
pub(crate) fn corpus_doi(doi: &str) -> Result<String, String> {
    match openalex_ingest::lookup::classify(doi) {
        openalex_ingest::lookup::Key::Doi(key) => Ok(key),
        openalex_ingest::lookup::Key::OpenAlexId(id) => Err(format!(
            "`doi` takes a DOI (`10.…`); {id} is an OpenAlex id"
        )),
    }
}

/// `conjunctive := 1` requires every query term to appear in the work.
/// Without it (the disjunctive default), a multi-term query against a
/// 56M-row corpus matches millions of works and DuckDB's BM25 runs out of
/// memory scoring them all. Every-term matching is also what a multi-word
/// search means.
const CORPUS_SEARCH_SQL: &str = r#"
    WITH ranked AS (
      SELECT
        openalex_id, doi, title, publication_year, cited_by_count,
        fts_main_works.match_bm25(openalex_id, ?, conjunctive := 1) AS bm25
      FROM works
    )
    SELECT openalex_id, doi, title, publication_year, cited_by_count, bm25
    FROM ranked
    WHERE bm25 IS NOT NULL
      AND (CAST(? AS BIGINT) IS NULL OR publication_year >= CAST(? AS BIGINT))
      AND (CAST(? AS BIGINT) IS NULL OR publication_year <= CAST(? AS BIGINT))
    ORDER BY bm25 DESC
    LIMIT ?;
"#;

const CORPUS_WORK_SQL: &str =
    "SELECT openalex_id, doi, title, abstract_text, publication_year, publication_date, \
            language, type, cited_by_count, is_retracted, is_oa, oa_url, primary_source_id \
     FROM works WHERE doi = ? LIMIT 1";

const CORPUS_WORK_AUTHORS_SQL: &str = "SELECT a.openalex_id, a.display_name, wa.author_position, \
            wa.raw_affiliation_string, wa.institution_id \
     FROM work_authorships wa \
     LEFT JOIN authors a ON a.openalex_id = wa.author_id \
     WHERE wa.work_id = ? \
     ORDER BY wa.author_position";

const CORPUS_WORK_TOPICS_SQL: &str = "SELECT t.openalex_id, t.display_name, wt.score \
     FROM work_topics wt \
     LEFT JOIN topics t ON t.openalex_id = wt.topic_id \
     WHERE wt.work_id = ? \
     ORDER BY wt.score DESC NULLS LAST";

/// Two rows at most: one is the topic, two is a name that two topics share.
const TOPIC_SQL: &str = "SELECT openalex_id, display_name FROM topics \
     WHERE openalex_id = ? OR lower(display_name) = lower(?) \
     ORDER BY openalex_id LIMIT 2";

const TOPIC_SUGGESTIONS_SQL: &str = "SELECT openalex_id, display_name FROM topics \
     WHERE contains(lower(display_name), lower(?)) \
     ORDER BY display_name LIMIT ?";

const TOPIC_AUTHORS_SQL: &str = "SELECT a.openalex_id, a.display_name, a.cited_by_count, \
            COUNT(*) AS papers_in_topic \
     FROM work_topics wt \
     JOIN work_authorships wa ON wa.work_id = wt.work_id \
     JOIN authors a ON a.openalex_id = wa.author_id \
     WHERE wt.topic_id = ? \
     GROUP BY a.openalex_id, a.display_name, a.cited_by_count \
     ORDER BY a.cited_by_count DESC NULLS LAST, a.openalex_id \
     LIMIT ?";

/// `T10001 (Name)` for each topic row, comma separated.
fn topic_names(rows: &[serde_json::Value]) -> String {
    rows.iter()
        .map(|row| {
            let field = |name: &str| row.get(name).and_then(|v| v.as_str()).unwrap_or("?");
            format!("{} ({})", field("openalex_id"), field("display_name"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl HomeStillMcp {
    /// Answer one `discover` call.
    pub(crate) async fn discover_works(&self, p: DiscoverParams) -> Result<String, String> {
        match plan(p)? {
            Request::ProviderSearch {
                query,
                year_from,
                year_to,
                limit,
            } => {
                let search = paper::models::SearchQuery {
                    query,
                    search_type: paper::models::SearchType::Keywords,
                    max_results: limit as usize,
                    offset: 0,
                    date_filter: year_filter(year_from, year_to)?,
                    sort_by: paper::models::SortBy::Relevance,
                    min_citations: None,
                };
                // One shape, always: the works plus the providers that
                // failed (empty when every provider answered). A search
                // where every provider failed is an error, so a partial
                // result is never mistaken for a complete one.
                let result = self
                    .providers
                    .provider(&paper::cli::ProviderArg::All)
                    .search_by_query(&search)
                    .await
                    .map_err(|e| format!("the provider search failed: {e}"))?;
                to_json(&serde_json::json!({
                    "source": "providers",
                    "works": result.papers,
                    "provider_failures": result.provider_failures,
                }))
            }
            Request::ProviderWork { doi } => {
                let work = self
                    .providers
                    .provider(&paper::cli::ProviderArg::All)
                    .get_by_doi(&doi)
                    .await
                    .map_err(|e| format!("the provider lookup of {doi} failed: {e}"))?
                    .ok_or_else(|| format!("no provider knows a work with DOI {doi}"))?;
                to_json(&serde_json::json!({ "source": "providers", "work": work }))
            }
            Request::CorpusSearch {
                query,
                year_from,
                year_to,
                limit,
            } => {
                let (from, to) = (year_from.map(i64::from), year_to.map(i64::from));
                let works = self
                    .openalex
                    .rows(
                        CORPUS_SEARCH_SQL,
                        vec![
                            Sql::Text(query),
                            opt_value(from),
                            opt_value(from),
                            opt_value(to),
                            opt_value(to),
                            Sql::BigInt(i64::from(limit)),
                        ],
                    )
                    .await?;
                to_json(&serde_json::json!({ "source": "openalex", "works": works }))
            }
            Request::CorpusWork { doi } => {
                let key = corpus_doi(&doi)?;
                let mut work = self
                    .openalex
                    .rows(CORPUS_WORK_SQL, vec![Sql::Text(key.clone())])
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        format!("the local OpenAlex corpus has no work with DOI {key}")
                    })?;
                let work_id = work
                    .get("openalex_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("the OpenAlex work with DOI {key} has no id"))?
                    .to_string();
                let authors = self
                    .openalex
                    .rows(CORPUS_WORK_AUTHORS_SQL, vec![Sql::Text(work_id.clone())])
                    .await?;
                let topics = self
                    .openalex
                    .rows(CORPUS_WORK_TOPICS_SQL, vec![Sql::Text(work_id)])
                    .await?;
                if let serde_json::Value::Object(fields) = &mut work {
                    fields.insert("authors".into(), serde_json::Value::Array(authors));
                    fields.insert("topics".into(), serde_json::Value::Array(topics));
                }
                to_json(&serde_json::json!({ "source": "openalex", "work": work }))
            }
            Request::CorpusAuthors { topic, limit } => {
                let found = self
                    .openalex
                    .rows(
                        TOPIC_SQL,
                        vec![Sql::Text(topic.to_uppercase()), Sql::Text(topic.clone())],
                    )
                    .await?;
                let topic_row = match found.as_slice() {
                    [one] => one.clone(),
                    [] => {
                        let near = self
                            .openalex
                            .rows(
                                TOPIC_SUGGESTIONS_SQL,
                                vec![Sql::Text(topic.clone()), Sql::BigInt(TOPIC_SUGGESTIONS)],
                            )
                            .await?;
                        return Err(if near.is_empty() {
                            format!(
                                "no OpenAlex topic has the id or the name {topic:?}, and no \
                                 topic's name contains it"
                            )
                        } else {
                            format!(
                                "no OpenAlex topic has the id or the name {topic:?}. Topics \
                                 whose name contains it: {}",
                                topic_names(&near)
                            )
                        });
                    }
                    several => {
                        return Err(format!(
                            "more than one OpenAlex topic is named {topic:?}: {}. Ask by id",
                            topic_names(several)
                        ));
                    }
                };
                let topic_id = topic_row
                    .get("openalex_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("the OpenAlex topic {topic:?} has no id"))?
                    .to_string();
                let authors = self
                    .openalex
                    .rows(
                        TOPIC_AUTHORS_SQL,
                        vec![Sql::Text(topic_id), Sql::BigInt(i64::from(limit))],
                    )
                    .await?;
                to_json(&serde_json::json!({
                    "source": "openalex",
                    "topic": topic_row,
                    "authors": authors,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(source: DiscoverSource) -> DiscoverParams {
        DiscoverParams {
            source,
            query: None,
            doi: None,
            kind: DiscoverKind::Works,
            year_from: None,
            year_to: None,
            limit: None,
        }
    }

    #[test]
    fn a_query_is_a_search_with_the_default_limit() {
        let request = plan(DiscoverParams {
            query: Some("  protein folding ".into()),
            year_from: Some(2020),
            ..params(DiscoverSource::Providers)
        })
        .unwrap();
        assert_eq!(
            request,
            Request::ProviderSearch {
                query: "protein folding".into(),
                year_from: Some(2020),
                year_to: None,
                limit: DISCOVER_DEFAULT_LIMIT,
            }
        );
    }

    #[test]
    fn a_doi_is_a_lookup_in_the_source_asked_for() {
        let request = plan(DiscoverParams {
            doi: Some("10.1234/abc".into()),
            ..params(DiscoverSource::Openalex)
        })
        .unwrap();
        assert_eq!(
            request,
            Request::CorpusWork {
                doi: "10.1234/abc".into()
            }
        );
    }

    #[test]
    fn query_and_doi_together_or_neither_are_refused() {
        let both = plan(DiscoverParams {
            query: Some("x".into()),
            doi: Some("10.1234/abc".into()),
            ..params(DiscoverSource::Providers)
        })
        .unwrap_err();
        assert!(both.contains("not both"), "{both}");
        let neither = plan(params(DiscoverSource::Providers)).unwrap_err();
        assert!(
            neither.contains("`query`") && neither.contains("`doi`"),
            "{neither}"
        );
    }

    #[test]
    fn a_blank_query_is_a_mistake_not_an_absent_one() {
        let err = plan(DiscoverParams {
            query: Some("   ".into()),
            doi: Some("10.1234/abc".into()),
            ..params(DiscoverSource::Providers)
        })
        .unwrap_err();
        assert_eq!(err, "`query` is empty");
    }

    #[test]
    fn a_limit_outside_the_range_is_refused_not_lowered() {
        for limit in [0, DISCOVER_MAX_LIMIT + 1] {
            let err = plan(DiscoverParams {
                query: Some("x".into()),
                limit: Some(limit),
                ..params(DiscoverSource::Openalex)
            })
            .unwrap_err();
            assert!(err.contains(&format!("got {limit}")), "{err}");
        }
    }

    #[test]
    fn search_filters_on_a_doi_lookup_are_refused() {
        let err = plan(DiscoverParams {
            doi: Some("10.1234/abc".into()),
            year_to: Some(2020),
            ..params(DiscoverSource::Providers)
        })
        .unwrap_err();
        assert!(err.contains("leave them out"), "{err}");
    }

    #[test]
    fn authors_need_the_corpus_a_query_and_no_years() {
        let authors = |source| DiscoverParams {
            kind: DiscoverKind::Authors,
            ..params(source)
        };
        let online = plan(DiscoverParams {
            query: Some("T1".into()),
            ..authors(DiscoverSource::Providers)
        })
        .unwrap_err();
        assert!(online.contains("`source: \"openalex\"`"), "{online}");
        let by_doi = plan(DiscoverParams {
            doi: Some("10.1234/abc".into()),
            ..authors(DiscoverSource::Openalex)
        })
        .unwrap_err();
        assert!(by_doi.contains("not a `doi`"), "{by_doi}");
        let dated = plan(DiscoverParams {
            query: Some("T1".into()),
            year_from: Some(2000),
            ..authors(DiscoverSource::Openalex)
        })
        .unwrap_err();
        assert!(dated.contains("over all years"), "{dated}");
        let asked = plan(DiscoverParams {
            query: Some("T1".into()),
            limit: Some(3),
            ..authors(DiscoverSource::Openalex)
        })
        .unwrap();
        assert_eq!(
            asked,
            Request::CorpusAuthors {
                topic: "T1".into(),
                limit: 3
            }
        );
    }

    #[test]
    fn a_reversed_year_range_is_refused() {
        let err = plan(DiscoverParams {
            query: Some("x".into()),
            year_from: Some(2021),
            year_to: Some(2019),
            ..params(DiscoverSource::Providers)
        })
        .unwrap_err();
        assert!(err.contains("2021") && err.contains("2019"), "{err}");
    }

    #[test]
    fn a_year_range_becomes_an_inclusive_date_filter() {
        assert!(year_filter(None, None).unwrap().is_none());
        assert!(year_filter(Some(2019), Some(2021)).unwrap().is_some());
    }

    #[test]
    fn an_openalex_id_is_not_a_doi() {
        assert_eq!(
            corpus_doi("https://doi.org/10.1234/ABC").unwrap(),
            "10.1234/abc"
        );
        let err = corpus_doi("W2741809807").unwrap_err();
        assert!(err.contains("OpenAlex id"), "{err}");
    }
}
