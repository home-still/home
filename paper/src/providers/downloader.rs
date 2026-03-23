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

#[derive(Deserialize)]
struct SemanticScholarResponse {
    #[serde(rename = "openAccessPdf")]
    open_access_pdf: Option<SemanticScholarPdf>,
}

#[derive(Deserialize)]
struct SemanticScholarPdf {
    url: String,
}

#[derive(Deserialize)]
struct EuropePmcResponse {
    #[serde(rename = "resultList")]
    result_list: EuropePmcResultList,
}

#[derive(Deserialize)]
struct EuropePmcResultList {
    result: Vec<EuropePmcResult>,
}

#[derive(Deserialize)]
struct EuropePmcResult {
    #[serde(rename = "fullTextUrlList")]
    full_text_url_list: Option<EuropePmcUrlList>,
}

#[derive(Deserialize)]
struct EuropePmcUrlList {
    #[serde(rename = "fullTextUrl")]
    full_text_url: Vec<EuropePmcUrl>,
}

#[derive(Deserialize)]
struct EuropePmcUrl {
    #[serde(rename = "documentStyle")]
    document_style: Option<String>,
    url: String,
}

#[derive(Deserialize)]
struct CoreSearchResponse {
    results: Vec<CoreOutput>,
}

#[derive(Deserialize)]
struct CoreOutput {
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
}
pub struct PaperDownloader {
    client: Client,
    download_path: PathBuf,
    unpaywall_email: Option<String>,
    core_api_key: Option<String>,
}

impl PaperDownloader {
    pub fn new(download_path: PathBuf, config: &DownloadConfig) -> Result<Self, PaperError> {
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
            core_api_key: config.core_api_key.clone(),
        })
    }

    async fn resolve_unpaywall(&self, doi: &str) -> Option<String> {
        let email = self.unpaywall_email.as_ref()?;
        let url = format!("https://api.unpaywall.org/v2/{}?email={}", doi, email);
        let response = self.client.get(&url).send().await.ok()?;
        let data: UnpaywallResponse = response.json().await.ok()?;
        data.best_oa_location?.url_for_pdf
    }

    async fn resolve_semantic_scholar(&self, doi: &str) -> Option<String> {
        let url = format!(
            "https://api.semanticscholar.org/graph/v1/paper/DOI:{}?fields=openAccessPdf",
            doi
        );
        let response = self.client.get(&url).send().await.ok()?;
        let data: SemanticScholarResponse = response.json().await.ok()?;
        let pdf_url = data.open_access_pdf?.url;
        if pdf_url.is_empty() {
            None
        } else {
            Some(pdf_url)
        }
    }

    async fn resolve_europe_pmc(&self, doi: &str) -> Option<String> {
        let url = format!(
            "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:{}&format=json&resultType=core",
            doi
        );
        let response = self.client.get(&url).send().await.ok()?;
        let data: EuropePmcResponse = response.json().await.ok()?;
        let result = data.result_list.result.into_iter().next()?;
        let urls = result.full_text_url_list?;
        urls.full_text_url
            .into_iter()
            .find(|u| u.document_style.as_deref() == Some("pdf"))
            .map(|u| u.url)
    }

    async fn resolve_core(&self, doi: &str) -> Option<String> {
        let api_key = self.core_api_key.as_ref()?;
        let url = format!(
            "https://api.core.ac.uk/v3/search/outputs/?q=doi:\"{}\"&limit=1",
            doi
        );
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .ok()?;
        let data: CoreSearchResponse = response.json().await.ok()?;
        data.results.into_iter().next()?.download_url
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

        // 3. Semantic Scholar lookup
        if let Some(pdf_url) = self.resolve_semantic_scholar(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 4. CORE Lookup
        if let Some(pdf_url) = self.resolve_europe_pmc(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 5. Europe PMC lookup
        if let Some(pdf_url) = self.resolve_core(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 6. No resolver found
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

        // Skip if already downloaded
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
            skipped: false,
        })
    }
}
