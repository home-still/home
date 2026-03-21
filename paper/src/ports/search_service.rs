use crate::error::PaperError;
use crate::models::{SearchQuery, SearchResult};
use async_trait::async_trait;

#[async_trait]
pub trait SearchService: Send + Sync {
    async fn search_all(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, PaperError>;

    async fn search_provider(
        &self,
        provider_name: &str,
        query: &SearchQuery,
    ) -> Result<SearchResult, PaperError>;
}
