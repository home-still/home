//! Thin facade over `hs_distill::client::DistillClient` that pins every call
//! to the personal collection. The distill server now accepts a per-request
//! `collection` parameter and lazy-creates the named collection on first use,
//! so a single distill daemon serves both `academic_papers` and `personal_docs`.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use hs_common::catalog::CatalogEntry;
use hs_distill::client::{DistillClient, IndexResult, SearchFilters, SearchHit};

pub struct PersonalDistill {
    inner: DistillClient,
    pub collection: String,
}

impl PersonalDistill {
    pub fn new(cfg: &Config) -> Result<Self> {
        let inner = DistillClient::new(&cfg.distill_url)
            .map_err(|e| PersonalError::Index(format!("distill client init: {e}")))?;
        Ok(Self {
            inner,
            collection: cfg.collection_name.clone(),
        })
    }

    pub async fn index(
        &self,
        path_hint: &str,
        markdown: &str,
        catalog: &CatalogEntry,
    ) -> Result<IndexResult> {
        self.inner
            .index_content_in(path_hint, markdown, Some(catalog), Some(&self.collection))
            .await
            .map_err(|e| PersonalError::Index(e.to_string()))
    }

    pub async fn delete(&self, doc_id: &str) -> Result<u64> {
        self.inner
            .delete_doc_in(doc_id, Some(&self.collection))
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
            .search_in(query, limit, filters, Some(&self.collection))
            .await
            .map_err(|e| PersonalError::Index(e.to_string()))
    }
}
