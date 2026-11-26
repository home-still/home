//! `graph`: one step along the citation graph from a DOI.
//!
//! `references` are the works a paper cites; `citations` are the works that
//! cite it. Two sources answer, and a call names one: Semantic Scholar's
//! Graph API (`semantic_scholar`), or the local OpenAlex corpus (`openalex`),
//! which needs no network.

use duckdb::types::Value as Sql;
use paper::providers::semantic_scholar::{CitationGraphEntry, CitationsOpts};
use rmcp::schemars;

use super::discover::corpus_doi;
use crate::openalex::opt_value;
use crate::{to_json, HomeStillMcp};

/// How many works a call returns when the caller names no `limit`.
pub const GRAPH_DEFAULT_LIMIT: u32 = 100;
/// The most works one call returns.
pub const GRAPH_MAX_LIMIT: u32 = 1000;

/// Which way to step from the paper.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GraphDirection {
    /// The works the paper cites.
    References,
    /// The works that cite the paper.
    Citations,
}

/// Who answers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GraphSource {
    /// Semantic Scholar's Graph API.
    SemanticScholar,
    /// The local OpenAlex corpus.
    Openalex,
}

/// The order of the works returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GraphSort {
    /// Newest first; works without a year last.
    #[default]
    Year,
    /// Most cited first; works without a count last.
    Citations,
}

impl GraphSort {
    /// The name Semantic Scholar's client knows the order by.
    fn semantic_scholar_name(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Citations => "citations",
        }
    }
}

/// The arguments of `graph`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GraphParams {
    #[schemars(description = "The DOI of the paper to step from.")]
    pub doi: String,
    #[schemars(
        description = "`references` (the works this paper cites) or `citations` (the works that cite it)."
    )]
    pub direction: GraphDirection,
    #[schemars(
        description = "`semantic_scholar` (the Graph API, online) or `openalex` (the local OpenAlex corpus)."
    )]
    pub source: GraphSource,
    #[schemars(description = "How many works to return: 1 to 1000, 100 by default.")]
    pub limit: Option<u32>,
    #[schemars(description = "Only works published in this year or later.")]
    pub year_from: Option<u16>,
    #[schemars(
        description = "`year` (newest first, the default) or `citations` (most cited first)."
    )]
    #[serde(default)]
    pub sort: GraphSort,
}

/// What `graph` answers. The works carry the fields their source has.
#[derive(Debug, serde::Serialize)]
struct Neighbours<W: serde::Serialize> {
    doi: String,
    direction: GraphDirection,
    source: GraphSource,
    works: Vec<W>,
    returned: usize,
    /// More works matched than were returned.
    truncated: bool,
    /// How many works the source says match, when it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_available: Option<u32>,
}

/// The number of works to return. A `limit` outside the range is refused:
/// lowering it silently would answer a smaller question than was asked.
fn result_limit(limit: Option<u32>) -> Result<u32, String> {
    match limit {
        None => Ok(GRAPH_DEFAULT_LIMIT),
        Some(n) if (1..=GRAPH_MAX_LIMIT).contains(&n) => Ok(n),
        Some(n) => Err(format!("`limit` must be 1 to {GRAPH_MAX_LIMIT}, got {n}")),
    }
}

/// A reference list as the caller asked for it. Semantic Scholar returns a
/// paper's whole list at once, so the year floor, the order and the limit
/// are applied here. Returns the works and whether the limit cut any off.
fn shape_references(
    mut entries: Vec<CitationGraphEntry>,
    year_from: Option<u16>,
    sort: GraphSort,
    limit: u32,
) -> (Vec<CitationGraphEntry>, bool) {
    // A work with no year cannot be shown to pass a floor.
    entries.retain(|entry| year_from.is_none_or(|floor| entry.year.is_some_and(|y| y >= floor)));
    // Stable, so works that tie keep the order of the reference list.
    match sort {
        GraphSort::Year => entries.sort_by_key(|entry| std::cmp::Reverse(entry.year)),
        GraphSort::Citations => {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.citation_count))
        }
    }
    let limit = limit as usize;
    let cut = entries.len() > limit;
    entries.truncate(limit);
    (entries, cut)
}

/// The works on the other end of the edges that touch one work, with the
/// edge column that names them. A work the corpus has an edge to but no row
/// for still comes back, with its id and nothing else.
fn corpus_sql(direction: GraphDirection, sort: GraphSort) -> String {
    let (other, this) = match direction {
        GraphDirection::References => ("referenced_work_id", "work_id"),
        GraphDirection::Citations => ("work_id", "referenced_work_id"),
    };
    let order = match sort {
        GraphSort::Year => "w.publication_year DESC NULLS LAST, w.cited_by_count DESC NULLS LAST",
        GraphSort::Citations => {
            "w.cited_by_count DESC NULLS LAST, w.publication_year DESC NULLS LAST"
        }
    };
    format!(
        "SELECT wr.{other} AS openalex_id, w.doi, w.title, w.publication_year, w.cited_by_count \
         FROM work_references wr \
         LEFT JOIN works w ON w.openalex_id = wr.{other} \
         WHERE wr.{this} = ? \
           AND (CAST(? AS BIGINT) IS NULL OR w.publication_year >= CAST(? AS BIGINT)) \
         ORDER BY {order}, wr.{other} \
         LIMIT ?"
    )
}

const CORPUS_WORK_ID_SQL: &str = "SELECT openalex_id FROM works WHERE doi = ? LIMIT 1";

impl HomeStillMcp {
    /// Answer one `graph` call.
    pub(crate) async fn graph_neighbours(&self, p: GraphParams) -> Result<String, String> {
        let limit = result_limit(p.limit)?;
        if p.doi.trim().is_empty() {
            return Err("`doi` is empty".to_string());
        }
        match p.source {
            GraphSource::SemanticScholar => self.semantic_scholar_neighbours(p, limit).await,
            GraphSource::Openalex => self.corpus_neighbours(p, limit).await,
        }
    }

    async fn semantic_scholar_neighbours(
        &self,
        p: GraphParams,
        limit: u32,
    ) -> Result<String, String> {
        let doi = paper::stem::normalize_doi(&p.doi).map_err(|e| format!("invalid DOI: {e}"))?;
        match p.direction {
            GraphDirection::References => {
                let answer = self
                    .providers
                    .references(&doi)
                    .await
                    .map_err(|e| format!("the references lookup of {doi} failed: {e}"))?;
                let (works, cut) = shape_references(answer.references, p.year_from, p.sort, limit);
                to_json(&Neighbours {
                    doi,
                    direction: p.direction,
                    source: p.source,
                    returned: works.len(),
                    works,
                    truncated: answer.truncated || cut,
                    total_available: None,
                })
            }
            GraphDirection::Citations => {
                let answer = self
                    .providers
                    .citations(
                        &doi,
                        CitationsOpts {
                            limit: Some(limit),
                            year_from: p.year_from,
                            sort: Some(p.sort.semantic_scholar_name().to_string()),
                        },
                    )
                    .await
                    .map_err(|e| format!("the citations lookup of {doi} failed: {e}"))?;
                to_json(&Neighbours {
                    doi,
                    direction: p.direction,
                    source: p.source,
                    returned: answer.citations.len(),
                    works: answer.citations,
                    truncated: answer.truncated,
                    total_available: answer.total_available,
                })
            }
        }
    }

    async fn corpus_neighbours(&self, p: GraphParams, limit: u32) -> Result<String, String> {
        let doi = corpus_doi(&p.doi)?;
        let work_id = self
            .openalex
            .rows(CORPUS_WORK_ID_SQL, vec![Sql::Text(doi.clone())])
            .await?
            .first()
            .and_then(|row| row.get("openalex_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("the local OpenAlex corpus has no work with DOI {doi}"))?
            .to_string();
        let floor = p.year_from.map(i64::from);
        // One row more than asked for says whether the limit cut any off.
        let mut works = self
            .openalex
            .rows(
                &corpus_sql(p.direction, p.sort),
                vec![
                    Sql::Text(work_id),
                    opt_value(floor),
                    opt_value(floor),
                    Sql::BigInt(i64::from(limit) + 1),
                ],
            )
            .await?;
        let limit = limit as usize;
        let truncated = works.len() > limit;
        works.truncate(limit);
        to_json(&Neighbours {
            doi,
            direction: p.direction,
            source: p.source,
            returned: works.len(),
            works,
            truncated,
            total_available: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, year: Option<u16>, citations: Option<u32>) -> CitationGraphEntry {
        CitationGraphEntry {
            doi: None,
            title: title.to_string(),
            year,
            authors: Vec::new(),
            venue: None,
            citation_count: citations,
            semantic_scholar_id: None,
        }
    }

    fn titles(entries: &[CitationGraphEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.title.as_str()).collect()
    }

    fn list() -> Vec<CitationGraphEntry> {
        vec![
            entry("old", Some(1999), Some(50)),
            entry("undated", None, Some(900)),
            entry("new", Some(2021), Some(5)),
            entry("uncounted", Some(2010), None),
        ]
    }

    #[test]
    fn references_are_newest_first_with_undated_last() {
        let (works, cut) = shape_references(list(), None, GraphSort::Year, GRAPH_DEFAULT_LIMIT);
        assert_eq!(titles(&works), ["new", "uncounted", "old", "undated"]);
        assert!(!cut);
    }

    #[test]
    fn references_by_citations_put_uncounted_last() {
        let (works, _) = shape_references(list(), None, GraphSort::Citations, GRAPH_DEFAULT_LIMIT);
        assert_eq!(titles(&works), ["undated", "old", "new", "uncounted"]);
    }

    #[test]
    fn a_year_floor_drops_older_and_undated_references() {
        let (works, cut) =
            shape_references(list(), Some(2010), GraphSort::Year, GRAPH_DEFAULT_LIMIT);
        assert_eq!(titles(&works), ["new", "uncounted"]);
        assert!(!cut);
    }

    #[test]
    fn the_limit_cuts_after_sorting_and_says_so() {
        let (works, cut) = shape_references(list(), None, GraphSort::Year, 1);
        assert_eq!(titles(&works), ["new"]);
        assert!(cut);
    }

    #[test]
    fn a_limit_outside_the_range_is_refused_not_lowered() {
        assert_eq!(result_limit(None).unwrap(), GRAPH_DEFAULT_LIMIT);
        assert_eq!(
            result_limit(Some(GRAPH_MAX_LIMIT)).unwrap(),
            GRAPH_MAX_LIMIT
        );
        for limit in [0, GRAPH_MAX_LIMIT + 1] {
            let err = result_limit(Some(limit)).unwrap_err();
            assert!(err.contains(&format!("got {limit}")), "{err}");
        }
    }
}
