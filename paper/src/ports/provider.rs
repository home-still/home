use crate::error::PaperError;
use crate::models::{SearchQuery, SearchResult, SearchType};
use async_trait::async_trait;

#[async_trait]
pub trait PaperProvider: Send + Sync {
    fn name(&self) -> &'static str;

    fn supported_search_types(&self) -> Vec<SearchType>;

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperError>;

    fn priority(&self) -> u8 {
        100
    }

    async fn get_by_doi(&self, _doi: &str) -> Result<Option<crate::models::Paper>, PaperError> {
        Ok(None)
    }

    async fn health_check(&self) -> Result<(), PaperError> {
        Ok(())
    }
}
