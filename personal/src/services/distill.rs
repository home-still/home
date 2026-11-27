//! Thin facade over `hs_distill::client::DistillClient` that pins every call
//! to the `personal` store of the registry (`hs_common::stores`). The distill
//! server files a document in the store its request names and answers HTTP
//! 400 for a name the registry does not hold.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use hs_common::catalog::CatalogEntry;
use hs_common::stores::PERSONAL;
use hs_distill::client::{DistillClient, IndexResult, SearchFilters, SearchHit};

pub struct PersonalDistill {
    inner: DistillClient,
}

impl PersonalDistill {
    pub fn new(cfg: &Config) -> Result<Self> {
        let inner = DistillClient::new(&cfg.distill_url)
            .map_err(|e| PersonalError::Index(format!("distill client init: {e}")))?;
        Ok(Self { inner })
    }

    pub async fn index(
        &self,
        path_hint: &str,
        markdown: &str,
        catalog: &CatalogEntry,
    ) -> Result<IndexResult> {
        self.inner
            .index_content(PERSONAL, path_hint, markdown, Some(catalog))
            .await
            .map_err(|e| PersonalError::Index(e.to_string()))
    }

    pub async fn delete(&self, doc_id: &str) -> Result<u64> {
        self.inner
            .delete_doc(PERSONAL, doc_id)
            .await
            .map_err(|e| PersonalError::Index(e.to_string()))
    }

    pub async fn search(
        &self,
        query: &str,
        limit: u64,
        category: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let filters = SearchFilters {
            category: category.map(|s| s.to_string()),
            ..Default::default()
        };
        self.inner
            .search(&[PERSONAL], query, limit, filters)
            .await
            .map_err(|e| PersonalError::Index(e.to_string()))
    }
}
