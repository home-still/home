use std::path::PathBuf;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{header, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::DownloadConfig;
use crate::error::PaperError;
use crate::models::DownloadResult;
use crate::ports::download_service::DownloadService;
use crate::ports::provider::PaperProvider;

#[derive(Deserialize)]
struct UnpaywallResponse {
    is_oa: bool,
    best_oa_location: Option<UnpaywallLocation>,
    oa_locations: Option<Vec<UnpaywallLocation>>,
}

#[derive(Deserialize)]
struct UnpaywallLocation {
    url_for_pdf: Option<String>,
    url_for_landing_page: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
    #[allow(dead_code)]
    license: Option<String>,
}

pub struct PaperDownloader {
    client: Client,
    download_path: PathBuf,
    unpaywall_email: Option<String>,
    resolvers: Vec<Box<dyn PaperProvider>>,
}

impl PaperDownloader {
    pub fn new(
        download_path: PathBuf,
        config: &DownloadConfig,
        resolvers: Vec<Box<dyn PaperProvider>>,
    ) -> Result<Self, PaperError> {
        let user_agent = match &config.unpaywall_email {
            Some(email) => format!(
                "{}/{} (mailto:{})",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                email
            ),
            None => format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        };

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/pdf,*/*"),
        );

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .user_agent(user_agent)
            .default_headers(headers)
            .build()?;

        Ok(Self {
            client,
            download_path,
            unpaywall_email: config.unpaywall_email.clone(),
            resolvers,
        })
    }

    async fn resolve_unpaywall(&self, doi: &str) -> Option<String> {
        let email = self.unpaywall_email.as_ref()?;
        let url = format!("https://api.unpaywall.org/v2/{}?email={}", doi, email);
        tracing::debug!(doi, "Unpaywall lookup");

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(doi, error = %e, "Unpaywall request failed");
                return None;
            }
        };

        if !response.status().is_success() {
            tracing::warn!(doi, status = %response.status(), "Unpaywall returned error");
            return None;
        }

        let data: UnpaywallResponse = match response.json().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(doi, error = %e, "Unpaywall response parse failed");
                return None;
            }
        };

        if !data.is_oa {
            tracing::debug!(doi, "Unpaywall: not open access");
            return None;
        }

        // 1. Best OA location PDF
        if let Some(ref loc) = data.best_oa_location {
            if let Some(ref pdf_url) = loc.url_for_pdf {
                tracing::debug!(doi, url = %pdf_url, "Unpaywall: best OA PDF");
                return Some(pdf_url.clone());
            }
        }

        // 2. Try all OA locations
        if let Some(locations) = &data.oa_locations {
            for loc in locations {
                if let Some(ref pdf_url) = loc.url_for_pdf {
                    tracing::debug!(doi, url = %pdf_url, version = ?loc.version, "Unpaywall: alternate OA PDF");
                    return Some(pdf_url.clone());
                }
            }
        }

        // 3. Landing page fallback (best location only)
        if let Some(ref loc) = data.best_oa_location {
            if let Some(ref landing) = loc.url_for_landing_page {
                tracing::debug!(doi, url = %landing, "Unpaywall: landing page fallback");
                return Some(landing.clone());
            }
        }

        tracing::debug!(doi, "Unpaywall: OA but no usable URL found");
        None
    }

    async fn resolve_via_providers(&self, doi: &str) -> Option<String> {
        for resolver in &self.resolvers {
            if let Ok(Some(paper)) = resolver.get_by_doi(doi).await {
                if let Some(url) = paper.download_urls.into_iter().next() {
                    return Some(url);
                }
            }
        }
        None
    }
}

#[async_trait]
impl DownloadService for PaperDownloader {
    async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError> {
        let filename = format!("{}.pdf", doi.replace('/', "_"));

        // 1. arXiv fast path
        if let Some(arxiv_id) = doi.strip_prefix("10.48550/arXiv.") {
            let url = format!("https://arxiv.org/pdf/{}", arxiv_id);
            if let Ok(result) = self.download_by_url(&url, &filename, None).await {
                return Ok(result);
            }
        }

        // 2. Unpaywall lookup
        if let Some(pdf_url) = self.resolve_unpaywall(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 3. Provider-based resolution (Semantic Scholar, Europe PMC, CORE, etc.)
        if let Some(pdf_url) = self.resolve_via_providers(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 4. No resolver found
        let detail = if self.unpaywall_email.is_some() {
            format!("No open-access PDF found for DOI: {}", doi)
        } else {
            format!(
                "No open-access PDF found for DOI: {}.  Set unpaywall_email in config to enable Unpaywall lookups.",
                doi
            )
        };
        Err(PaperError::NotFound(detail))
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
        let tmp_path = self.download_path.join(format!("{filename}.tmp"));

        // Skip if already downloaded (final file exists)
        if file_path.exists() {
            let metadata = tokio::fs::metadata(&file_path).await?;
            return Ok(DownloadResult {
                file_path,
                doi: None,
                sha256: String::new(),
                size_bytes: metadata.len(),
                skipped: true,
            });
        }

        // Stream to temp file, then atomic rename
        let response = self.client.get(url).send().await?.error_for_status()?;

        let content_length = response.content_length();

        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut hasher = Sha256::new();
        let mut size_bytes: u64 = 0;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            hasher.update(&bytes);
            size_bytes += bytes.len() as u64;
            file.write_all(&bytes).await?;

            if let Some(cb) = &on_progress {
                cb(size_bytes, content_length);
            }
        }

        file.flush().await?;
        drop(file); // close before rename

        // Atomic rename — safe on NFS within same directory
        tokio::fs::rename(&tmp_path, &file_path).await?;

        let sha256 = format!("{:x}", hasher.finalize());

        Ok(DownloadResult {
            file_path,
            doi: None,
            sha256,
            size_bytes,
            skipped: false,
        })
    }
}
