use std::path::PathBuf;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::DownloadConfig;
use crate::error::PaperError;
use crate::models::DownloadResult;
use crate::ports::download_service::DownloadService;

pub struct PaperDownloader {
    client: Client,
    download_path: PathBuf,
}

impl PaperDownloader {
    pub fn new(download_path: PathBuf, config: &DownloadConfig) -> Result<Self, PaperError> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()?;

        Ok(Self {
            client,
            download_path,
        })
    }
}

#[async_trait]
impl DownloadService for PaperDownloader {
    async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError> {
        // TODO: Handle other providers

        let arxiv_id = doi.strip_prefix("10.48550/arXiv.").ok_or_else(|| {
            PaperError::NotFound(format!("Cannot resolve download URL for DOI: {}", doi))
        })?;

        let url = format!("https://arxiv.org/pdf/{}", arxiv_id);
        let filename = format!("{}.pdf", doi.replace('/', "_"));
        self.download_by_url(&url, &filename, None).await
    }

    async fn download_by_url(
        &self,
        url: &str,
        filename: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError> {
        // Ensure download directory exists
        tokio::fs::create_dir_all(&self.download_path).await?;

        let file_path = self.download_path.join(filename);

        // Stream the response
        let response = self.client.get(url).send().await?.error_for_status()?;

        let content_length = response.content_length();

        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(&file_path).await?;
        let mut hasher = Sha256::new();
        let mut size_bytes: u64 = 0;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            hasher.update(&bytes);
            size_bytes += bytes.len() as u64;
            file.write_all(&bytes).await?;

            // Report byte level progress
            if let Some(cb) = &on_progress {
                cb(size_bytes, content_length);
            }
        }

        file.flush().await?;

        let sha256 = format!("{:x}", hasher.finalize());

        Ok(DownloadResult {
            file_path,
            doi: None,
            sha256,
            size_bytes,
        })
    }
}
