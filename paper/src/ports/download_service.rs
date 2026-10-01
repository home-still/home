use crate::error::PaperError;
use crate::models::DownloadResult;
use async_trait::async_trait;

#[async_trait]
pub trait DownloadService: Send + Sync {
    /// Download the PDF for `doi`, trying the ordered source chain. The
    /// storage identity is derived from the DOI by [`crate::stem::doi_stem`].
    async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError>;

    /// Download the PDF at `url` and store it under `stem` (a validated
    /// storage stem — see [`crate::stem`] — not a filename).
    async fn download_by_url(
        &self,
        url: &str,
        stem: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError>;
}
