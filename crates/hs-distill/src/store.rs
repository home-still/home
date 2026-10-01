//! The vector-store seam. The server talks to Qdrant only through
//! [`VectorStore`]; tests drive the same code with an in-memory fake.

use async_trait::async_trait;

use crate::client::{SearchFilters, SearchHit};
use crate::collection::CollectionSpec;
use crate::error::DistillError;
use crate::types::{EmbeddedChunk, ScrubReport};

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

/// What `enable_hnsw` did for one collection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HnswEnable {
    pub collection: String,
    /// An update was sent to Qdrant. False: the collection already had these
    /// parameters and nothing was changed.
    pub submitted: bool,
    pub m: u64,
    pub ef_construct: u64,
    /// Cap on index-building threads included in the update (0 when
    /// nothing was submitted).
    pub max_indexing_threads: u64,
    /// The graph builds in the background after this returns.
    pub message: String,
}

/// Does a collection with (`m`, `ef_construct`) already match the wanted
/// HNSW parameters? Unknown (`None`) never matches.
pub fn hnsw_matches(
    m: Option<u64>,
    ef_construct: Option<u64>,
    want: &crate::config::HnswConfig,
) -> bool {
    m == Some(want.m) && ef_construct == Some(want.ef_construct)
}

/// Distinct document ids of a collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocIds {
    pub ids: Vec<String>,
    /// More documents exist than `ids` holds (the requested limit was hit).
    pub truncated: bool,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Qdrant reachable; returns its version.
    async fn health(&self) -> Result<String, DistillError>;

    /// Insert or overwrite points (ids derive from `doc_id` + chunk index).
    async fn upsert(&self, collection: &str, chunks: &[EmbeddedChunk]) -> Result<(), DistillError>;

    /// Delete every point of `doc_id` whose `chunk_index >= from_chunk`.
    /// `from_chunk == 0` removes the whole document.
    async fn delete_chunks_from(
        &self,
        collection: &str,
        doc_id: &str,
        from_chunk: u32,
    ) -> Result<(), DistillError>;

    /// Number of points stored for `doc_id`.
    async fn doc_chunks(&self, collection: &str, doc_id: &str) -> Result<u64, DistillError>;

    async fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError>;

    /// Total points in the collection.
    async fn points_count(&self, collection: &str) -> Result<u64, DistillError>;

    /// Up to `limit` distinct document ids, flagging whether more exist.
    async fn doc_ids(&self, collection: &str, limit: u64) -> Result<DocIds, DistillError>;

    /// Submit `hnsw` (m, ef_construct, bounded indexing threads) to an
    /// existing collection and return immediately; Qdrant builds the graph
    /// in the background. A no-op when the collection already matches.
    async fn enable_hnsw(
        &self,
        collection: &str,
        hnsw: &crate::config::HnswConfig,
    ) -> Result<HnswEnable, DistillError>;

    /// Drop and recreate the collection; returns the prior point count.
    async fn reset(&self, collection: &str, spec: &CollectionSpec) -> Result<u64, DistillError>;

    /// Find (and unless `dry_run`, delete) chunks that are anti-bot pages.
    async fn scrub_interstitials(
        &self,
        collection: &str,
        dry_run: bool,
    ) -> Result<ScrubReport, DistillError>;
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
