use crate::error::PaperError;
use crate::models::DownloadResult;
use async_trait::async_trait;

#[async_trait]
pub trait DownloadService: Send + Sync {
    async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError>;

    async fn download_by_url(
        &self,
        url: &str,
        filename: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError>;
}
