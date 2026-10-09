use crate::error::PaperError;
use crate::models::{Paper, SearchQuery, SearchResult, SearchType};
use async_trait::async_trait;

#[async_trait]
pub trait PaperProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn supported_search_types(&self) -> Vec<SearchType>;

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError>;

    fn priority(&self) -> u8 {
        100
    }

    /// Whether `search_by_query` can honour a non-zero `SearchQuery::offset`.
    /// A provider that pages by an opaque cursor says `false` and rejects an
    /// offset; an aggregate search leaves such providers out of an offset
    /// query instead of fanning out to a guaranteed failure.
    fn supports_offset(&self) -> bool {
        true
    }

    async fn get_by_doi(&self, _doi: &str) -> Result<Option<Paper>, PaperError> {
        Ok(None)
    }

    /// A fan-out gave up on this provider because it overran its deadline.
    /// The call's future was dropped mid-flight, so nothing inside it could
    /// record the failure; a guarded provider counts it against its circuit
    /// breaker here (otherwise a provider that hangs is retried at full
    /// price forever). The default does nothing.
    fn note_timeout(&self) {}

    async fn health_check(&self) -> Result<(), PaperError> {
        Ok(())
    }
}

/// The answer to a `SearchType::DOI` search: the one paper `get_by_doi`
/// found, or none. A DOI is an identifier, not search text, so providers'
/// free-text endpoints are never asked for it.
pub fn doi_search_result(provider: &str, paper: Option<Paper>) -> SearchResult {
    SearchResult {
        total_results: usize::from(paper.is_some()),
        papers: paper.into_iter().collect(),
        next_offset: None,
        provider: provider.to_string(),
        provider_failures: Vec::new(),
    }
}
