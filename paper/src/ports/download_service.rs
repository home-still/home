use crate::error::PaperError;
use crate::models::DownloadResult;
use async_trait::async_trait;

#[async_trait]
pub trait DownloadService: Send + Sync {
    /// Download the PDF for `doi`, trying the ordered source chain. The
    /// storage identity is derived from the DOI by [`crate::stem::doi_stem`].
    ///
    /// `on_progress` receives `(bytes_downloaded, bytes_total)` per chunk,
    /// exactly as [`Self::download_by_url`] does. It is called only for the
    /// source that is actually transferring — a source that fails and falls
    /// through to the next one restarts the count from zero, which is the
    /// truth about the bytes on the wire.
    async fn download_by_doi(
        &self,
        doi: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError>;

    /// Download the PDF at `url` and store it under `stem` (a validated
    /// storage stem — see [`crate::stem`] — not a filename).
    async fn download_by_url(
        &self,
        url: &str,
        stem: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError>;
}
