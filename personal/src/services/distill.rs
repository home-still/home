//! Thin facade over `hs_distill::client::DistillClient` that pins every call
//! to the personal collection. The distill server accepts a per-request
//! `collection` parameter for the collections it is configured to serve
//! (`distill_server.collections`, which includes `personal_docs` by default)
//! and answers HTTP 400 for any other name, so `personal.collection_name`
//! must be listed there when it is not the default.

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
