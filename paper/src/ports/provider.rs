use crate::error::PaperError;
use crate::models::{SearchQuery, SearchResult, SearchType};
use async_trait::async_trait;

#[async_trait]
pub trait PaperProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn supported_search_types(&self) -> Vec<SearchType>;

    async fn search_by_query(&self, query: &SearchQuery) -> Result<SearchResult, PaperError>;

    fn priority(&self) -> u8 {
        100
    }

    async fn get_by_doi(&self, _doi: &str) -> Result<Option<crate::models::Paper>, PaperError> {
        Ok(None)
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError> {
        if matches!(query.search_type, SearchType::DOI) {
            let paper = self.get_by_doi(&query.query).await?;
            return Ok(SearchResult {
                total_results: if paper.is_some() { 1 } else { 0 },
                papers: paper.into_iter().collect(),
                next_offset: None,
                provider: self.name().to_string(),
            });
        }
        self.search_by_query(query).await
    }

    async fn health_check(&self) -> Result<(), PaperError> {
        Ok(())
    }
}
