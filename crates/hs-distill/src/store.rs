//! The document-store seam: a parsed search filter and the [`DocumentStore`]
//! trait the server writes and searches through.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::client::{SearchFilters, SearchHit};
use crate::error::DistillError;
use crate::types::Chunk;

/// A parsed `year` search filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YearFilter {
    Eq(i64),
    Gt(i64),
    Gte(i64),
    Lt(i64),
    Lte(i64),
}

impl YearFilter {
    /// Parse `"2023"`, `">2020"`, `">=2021"`, `"<2019"`, `"<=2018"`. Anything
    /// else is an error: a filter that cannot be applied must fail the
    /// request, not silently return unfiltered results.
    pub fn parse(s: &str) -> Result<Self, DistillError> {
        let s = s.trim();
        let bad = || {
            DistillError::InvalidInput(format!(
                "year filter {s:?} is not a year or a comparison like \">2020\", \">=2021\", \"<2019\", \"<=2018\""
            ))
        };
        let (ctor, rest): (fn(i64) -> YearFilter, &str) = if let Some(r) = s.strip_prefix(">=") {
            (YearFilter::Gte, r)
        } else if let Some(r) = s.strip_prefix("<=") {
            (YearFilter::Lte, r)
        } else if let Some(r) = s.strip_prefix('>') {
            (YearFilter::Gt, r)
        } else if let Some(r) = s.strip_prefix('<') {
            (YearFilter::Lt, r)
        } else {
            (YearFilter::Eq, s)
        };
        let year: i64 = rest.trim().parse().map_err(|_| bad())?;
        Ok(ctor(year))
    }
}

/// A validated search filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilter {
    pub year: Option<YearFilter>,
    pub topic: Option<String>,
    pub category: Option<String>,
}

impl SearchFilter {
    /// Validate the wire filters of a search request.
    pub fn from_request(filters: Option<&SearchFilters>) -> Result<Self, DistillError> {
        let Some(f) = filters else {
            return Ok(Self::default());
        };
        Ok(Self {
            year: f.year.as_deref().map(YearFilter::parse).transpose()?,
            topic: f.topic.clone(),
            category: f.category.clone(),
        })
    }
}

/// The document-store seam. The server talks to myelin only through
/// [`DocumentStore`]; tests drive the same code with an in-memory fake.
///
/// A document is one generation of records: [`write_document`] stores every
/// chunk of the document's current text and returns the record ids, and
/// [`forget_document`] with those ids as `keep` then removes every earlier
/// generation. The two calls are separate so a failed write never removes the
/// previous version.
///
/// [`write_document`]: DocumentStore::write_document
/// [`forget_document`]: DocumentStore::forget_document
#[async_trait]
pub trait DocumentStore: Send + Sync {
    /// Backend reachable; returns "<server name> <version>" from the MCP
    /// initialize result.
    async fn health(&self) -> Result<String, DistillError>;

    /// Write one generation of a document; returns one record id per chunk,
    /// in chunk order.
    async fn write_document(
        &self,
        collection: &str,
        doc_id: &str,
        chunks: &[Chunk],
    ) -> Result<Vec<String>, DistillError>;

    /// Hard-delete every record of `doc_id` not in `keep`; returns how many
    /// were deleted.
    async fn forget_document(
        &self,
        collection: &str,
        doc_id: &str,
        keep: &[String],
    ) -> Result<u64, DistillError>;

    /// Ranked hits, best first, at most `limit`.
    async fn search(
        &self,
        collection: &str,
        query: &str,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError>;

    /// doc_id -> live record count (one doc when `doc_id` is `Some`).
    async fn inventory(
        &self,
        collection: &str,
        doc_id: Option<&str>,
    ) -> Result<BTreeMap<String, u64>, DistillError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_filter_parses_every_documented_form() {
        assert_eq!(YearFilter::parse("2023").unwrap(), YearFilter::Eq(2023));
        assert_eq!(YearFilter::parse(" 2023 ").unwrap(), YearFilter::Eq(2023));
        assert_eq!(YearFilter::parse(">2020").unwrap(), YearFilter::Gt(2020));
        assert_eq!(YearFilter::parse(">=2021").unwrap(), YearFilter::Gte(2021));
        assert_eq!(YearFilter::parse("<2019").unwrap(), YearFilter::Lt(2019));
        assert_eq!(YearFilter::parse("<=2018").unwrap(), YearFilter::Lte(2018));
        assert_eq!(YearFilter::parse("> 2020").unwrap(), YearFilter::Gt(2020));
    }

    #[test]
    fn unparseable_year_filter_is_invalid_input_not_dropped() {
        for bad in [
            "",
            "abc",
            ">",
            ">=",
            "20x0",
            "2020-2022",
            ">2020.5",
            "=2020",
            "<>2020",
        ] {
            assert!(
                matches!(YearFilter::parse(bad), Err(DistillError::InvalidInput(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn filters_are_validated_as_a_set() {
        let ok = SearchFilter::from_request(Some(&SearchFilters {
            year: Some(">2020".into()),
            topic: Some("autism".into()),
            category: None,
        }))
        .unwrap();
        assert_eq!(ok.year, Some(YearFilter::Gt(2020)));
        assert_eq!(ok.topic.as_deref(), Some("autism"));

        assert_eq!(
            SearchFilter::from_request(None).unwrap(),
            SearchFilter::default()
        );

        let err = SearchFilter::from_request(Some(&SearchFilters {
            year: Some("soon".into()),
            topic: Some("autism".into()),
            category: None,
        }))
        .unwrap_err();
        assert!(matches!(err, DistillError::InvalidInput(_)));
    }
}
