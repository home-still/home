use std::path::PathBuf;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::DownloadConfig;
use crate::error::PaperError;
use crate::models::DownloadResult;
use crate::ports::download_service::DownloadService;

#[derive(Deserialize)]
#[allow(dead_code)]
struct UnpaywallResponse {
    is_oa: bool,
    best_oa_location: Option<UnpaywallLocation>,
}

#[derive(Deserialize)]
struct UnpaywallLocation {
    url_for_pdf: Option<String>,
}

pub struct PaperDownloader {
    client: Client,
    download_path: PathBuf,
    unpaywall_email: Option<String>,
}

impl PaperDownloader {
    pub fn new(download_path: PathBuf, config: &DownloadConfig) -> Result<Self, PaperError> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()?;

        Ok(Self {
            client,
            download_path,
            unpaywall_email: config.unpaywall_email.clone(),
        })
    }

    async fn resolve_unpaywall(&self, doi: &str) -> Option<String> {
        let email = self.unpaywall_email.as_ref()?;
        let url = format!("https://api.unpaywall.org/v2/{}?email={}", doi, email);
        let response = self.client.get(&url).send().await.ok()?;
        let data: UnpaywallResponse = response.json().await.ok()?;
        data.best_oa_location?.url_for_pdf
    }
}

#[async_trait]
impl DownloadService for PaperDownloader {
                                                                                               
  async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError> {               
      let filename = format!("{}.pdf", doi.replace('/', "_"));    
                                                                                                   
      // 1. arXiv fast path
      if let Some(arxiv_id) = doi.strip_prefix("10.48550/arXiv.") {                                
          let url = format!("https://arxiv.org/pdf/{}", arxiv_id);                                 
          return self.download_by_url(&url, &filename, None).await;
      }                                                                                            
                                                                  
      // 2. Unpaywall lookup                                                                       
      if let Some(pdf_url) = self.resolve_unpaywall(doi).await {  
          return self.download_by_url(&pdf_url, &filename, None).await;
      }                                                                                            
                                                                                                   
      // 3. No resolver found                                                                      
      Err(PaperError::NotFound(format!(                                                            
          "No open-access PDF found for DOI: {}.  Set unpaywall_email in config to enable Unpaywall lookups.",                                                                                      
          doi                                                                                      
      )))                                                                                          
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
