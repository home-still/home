use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use hs_common::event_bus::{EventBus, NoOpBus};
use hs_common::storage::Storage;
use reqwest::{header, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::DownloadConfig;
use crate::error::PaperError;
use crate::models::DownloadResult;
use crate::ports::download_service::DownloadService;
use crate::ports::provider::PaperProvider;

/// If `doi` is a DataCite-registered arXiv DOI of the form
/// `10.48550/arXiv.<id>` (any casing of `arXiv`), return the bare `<id>`.
/// Otherwise `None`. Used by both the download fast-path and the aggregate
/// `get_by_doi` router so an arXiv DOI always resolves to arXiv, not to the
/// other providers (which don't index this DOI prefix).
pub fn strip_arxiv_doi_prefix(doi: &str) -> Option<&str> {
    const PREFIX: &str = "10.48550/arxiv.";
    if doi.len() <= PREFIX.len() {
        return None;
    }
    if doi[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        Some(&doi[PREFIX.len()..])
    } else {
        None
    }
}

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

#[derive(Deserialize)]
struct PmcIdConverterResponse {
    records: Vec<PmcIdRecord>,
}

#[derive(Deserialize)]
struct PmcIdRecord {
    pmcid: Option<String>,
}

pub struct PaperDownloader {
    client: Client,
    storage: Arc<dyn Storage>,
    events: Arc<dyn EventBus>,
    unpaywall_email: Option<String>,
    /// Storage prefix all downloads land under (e.g. `"papers"`). The key
    /// for a downloaded artifact is `{papers_prefix}/{sharded_key(stem, ext)}`.
    /// Pre-rc.298 the prefix was silently omitted, scattering files across
    /// bucket-root shards — this field exists so writes and pipeline counts
    /// can't drift apart again.
    papers_prefix: String,
    resolvers: Vec<Box<dyn PaperProvider>>,
}

impl PaperDownloader {
    pub fn new(
        storage: Arc<dyn Storage>,
        config: &DownloadConfig,
        resolvers: Vec<Box<dyn PaperProvider>>,
    ) -> Result<Self, PaperError> {
        Self::with_event_bus(storage, Arc::new(NoOpBus), config, resolvers)
    }

    pub fn with_event_bus(
        storage: Arc<dyn Storage>,
        events: Arc<dyn EventBus>,
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
            storage,
            events,
            unpaywall_email: config.unpaywall_email.clone(),
            papers_prefix: config.papers_prefix.clone(),
            resolvers,
        })
    }

    /// Build the canonical storage key for a downloaded artifact:
    /// `{papers_prefix}/{XX}/{stem}.{ext}`. Centralised so the download
    /// path and tests share one definition and can't drift.
    fn build_key(&self, stem: &str, ext: &str) -> String {
        format!(
            "{}/{}",
            self.papers_prefix.trim_end_matches('/'),
            hs_common::sharded_key(stem, ext),
        )
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

    async fn resolve_pmc_direct(&self, doi: &str) -> Option<String> {
        let email = self
            .unpaywall_email
            .as_deref()
            .unwrap_or("home-still-user@example.com");
        let url = format!(
            "https://www.ncbi.nlm.nih.gov/pmc/utils/idconv/v1.0/?ids={}&format=json&tool=home-still&email={}",
            doi, email
        );
        tracing::debug!(doi, "PMC ID Converter lookup");

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(doi, error = %e, "PMC ID Converter request failed");
                return None;
            }
        };

        if !response.status().is_success() {
            tracing::warn!(doi, status = %response.status(), "PMC ID Converter returned error");
            return None;
        }

        let data: PmcIdConverterResponse = match response.json().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(doi, error = %e, "PMC ID Converter parse failed");
                return None;
            }
        };

        let pmcid = data.records.into_iter().find_map(|r| r.pmcid)?;
        let pdf_url = format!("https://pmc.ncbi.nlm.nih.gov/articles/{}/pdf/", pmcid);
        tracing::debug!(doi, pmcid = %pmcid, url = %pdf_url, "PMC direct PDF");
        Some(pdf_url)
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

        // 1. arXiv fast path — match the arXiv DOI prefix case-insensitively
        // (DataCite registration is `arXiv`, but consumers paste both
        // `arXiv` and `arxiv`; we should resolve either).
        if let Some(arxiv_id) = strip_arxiv_doi_prefix(doi) {
            let url = format!("https://arxiv.org/pdf/{}", arxiv_id);
            if let Ok(result) = self.download_by_url(&url, &filename, None).await {
                return Ok(result);
            }
        }

        // 1b. MDPI fast path — all MDPI journals are open access
        if doi.starts_with("10.3390/") {
            let url = format!("https://www.mdpi.com/{}/pdf", doi);
            tracing::debug!(doi, url = %url, "MDPI direct PDF");
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

        // 2b. PMC direct PDF (DOI → PMCID via NCBI ID Converter → PDF URL)
        if let Some(pdf_url) = self.resolve_pmc_direct(doi).await {
            if let Ok(result) = self.download_by_url(&pdf_url, &filename, None).await {
                return Ok(result);
            }
        }

        // 3. Provider-based resolution (Semantic Scholar, Europe PMC, CORE, OpenAlex, CrossRef)
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
        // Derive stem from filename for sharded directory layout
        let stem = filename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(filename);
        let ext = filename.rsplit_once('.').map(|(_, e)| e).unwrap_or("pdf");
        let key = self.build_key(stem, ext);

        // Skip if already downloaded
        if let Some(meta) = self
            .storage
            .head(&key)
            .await
            .map_err(|e| PaperError::Io(std::io::Error::other(e.to_string())))?
        {
            return Ok(DownloadResult {
                file_path: PathBuf::from(&key),
                doi: None,
                sha256: String::new(),
                size_bytes: meta.size,
                skipped: true,
            });
        }

        // Stream into memory, hashing as we go. S3 PUT is atomic; the local
        // backend writes through a parent-mkdir + fs::write.
        let response = self.client.get(url).send().await?.error_for_status()?;
        let content_length = response.content_length();
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = match content_length {
            Some(n) => Vec::with_capacity(n as usize),
            None => Vec::new(),
        };
        let mut hasher = Sha256::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            hasher.update(&bytes);
            buf.extend_from_slice(&bytes);
            if let Some(cb) = &on_progress {
                cb(buf.len() as u64, content_length);
            }
        }

        let size_bytes = buf.len() as u64;
        let sha256 = format!("{:x}", hasher.finalize());

        // Reject sub-`MIN_PDF_BYTES` bodies before they pollute the catalog.
        // The smallest valid PDF is ~70 bytes (`%PDF-1.x` + xref + trailer);
        // 100 is well below that. Catches 0-byte stubs (servers that returned
        // 200 with empty body) and HTML error pages that came through as
        // empty after gzip-strip. Without this gate, a 0-byte stub would be
        // stamped `downloaded: true` with sha256 of the empty string.
        const MIN_PDF_BYTES: u64 = 100;
        if size_bytes < MIN_PDF_BYTES {
            return Err(PaperError::NotFound(format!(
                "Server returned {size_bytes} bytes (< {MIN_PDF_BYTES}); rejecting as stub for {url}"
            )));
        }

        // Validate and decide final key
        let head = &buf[..buf.len().min(4096)];
        let (final_key, final_bytes) = if head.starts_with(b"%PDF") {
            (key, buf)
        } else if hs_common::html::looks_like_html(head) {
            let content = String::from_utf8_lossy(&buf).to_string();
            if hs_common::html::is_paywall_html(&content) {
                return Err(PaperError::NotFound(format!(
                    "Server returned a paywall/login page instead of PDF for {url}"
                )));
            }
            (self.build_key(stem, "html"), buf)
        } else {
            // Anything that's not %PDF and not HTML is garbage by the time
            // it reaches a paper-download response (graphical-abstract JPEG,
            // gzipped landing page, login binary blob). Storing under the
            // expected PDF key inflates `documents` and triggers convert
            // dispatch that fails 415 every time. Reject at the door —
            // catalog isn't stamped, file isn't written.
            return Err(PaperError::NotFound(format!(
                "downloaded body is neither PDF nor HTML ({size_bytes} bytes); rejecting for {url}"
            )));
        };

        self.storage
            .put(&final_key, final_bytes)
            .await
            .map_err(|e| PaperError::Io(std::io::Error::other(e.to_string())))?;

        // Confirm the object is queryable + correctly sized before declaring
        // success. `put()` returning Ok is not always sufficient on
        // S3-compatible backends (Garage etc.); without this check, a
        // truncated or evicted object would still be stamped as
        // `downloaded: true` in the catalog and then explode at scribe time
        // with "No PDF or HTML found for <stem>".
        verify_put(self.storage.head(&final_key).await, &final_key, size_bytes)?;

        // Announce the new artifact so scribe (or any other subscriber) can
        // pick it up. On NoOpBus this is a cheap no-op; with NATS it reaches
        // every subscriber on `papers.ingested`.
        let payload = serde_json::json!({
            "key": final_key,
            "sha256": sha256,
            "size_bytes": size_bytes,
            "source": "paper-download",
        });
        if let Err(e) = self
            .events
            .publish(
                "papers.ingested",
                serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
            )
            .await
        {
            // Publish failure shouldn't fail the download — the file is
            // safely in storage. Log and move on; a reconcile pass can
            // backfill missed events later.
            tracing::warn!(key = %final_key, error = %e, "event publish failed");
        }

        Ok(DownloadResult {
            file_path: PathBuf::from(&final_key),
            doi: None,
            sha256,
            size_bytes,
            skipped: false,
        })
    }
}

/// Check that a `head()` result confirms the just-written object exists at the
/// expected size. Pulled out so the mismatch/missing/error branches can be
/// covered without standing up a mock HTTP server for the full download path.
fn verify_put(
    head_result: anyhow::Result<Option<hs_common::storage::ObjectMeta>>,
    key: &str,
    expected_size: u64,
) -> Result<(), PaperError> {
    match head_result {
        Ok(Some(meta)) if meta.size == expected_size => Ok(()),
        Ok(Some(meta)) => Err(PaperError::Io(std::io::Error::other(format!(
            "post-write verify: size mismatch for {key} (wrote {expected_size}, head reports {})",
            meta.size
        )))),
        Ok(None) => Err(PaperError::Io(std::io::Error::other(format!(
            "post-write verify: {key} not found after put"
        )))),
        Err(e) => Err(PaperError::Io(std::io::Error::other(format!(
            "post-write verify failed for {key}: {e}"
        )))),
    }
}

#[cfg(test)]
mod verify_put_tests {
    use super::verify_put;
    use hs_common::storage::ObjectMeta;

    fn meta(size: u64) -> ObjectMeta {
        ObjectMeta {
            key: "k".into(),
            size,
            last_modified: None,
            etag: None,
        }
    }

    #[test]
    fn ok_when_head_returns_matching_size() {
        verify_put(Ok(Some(meta(123))), "k", 123).expect("matching size should pass");
    }

    #[test]
    fn err_on_size_mismatch() {
        let err = verify_put(Ok(Some(meta(99))), "k", 123).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("size mismatch"), "got: {msg}");
        assert!(msg.contains("99") && msg.contains("123"), "got: {msg}");
    }

    #[test]
    fn err_when_head_returns_none() {
        let err = verify_put(Ok(None), "k", 1).unwrap_err();
        assert!(format!("{err}").contains("not found after put"));
    }

    #[test]
    fn err_when_head_call_itself_fails() {
        let err = verify_put(Err(anyhow::anyhow!("network")), "k", 1).unwrap_err();
        assert!(format!("{err}").contains("verify failed"));
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use hs_common::event_bus::NoOpBus;
    use hs_common::storage::LocalFsStorage;

    fn mk(papers_prefix: &str) -> PaperDownloader {
        let tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(tmp.path()));
        let events: Arc<dyn EventBus> = Arc::new(NoOpBus);
        let cfg = crate::config::DownloadConfig {
            papers_prefix: papers_prefix.to_string(),
            ..Default::default()
        };
        PaperDownloader::with_event_bus(storage, events, &cfg, Vec::new()).unwrap()
    }

    #[test]
    fn keys_sit_under_default_papers_prefix() {
        // rc.298 guard: the pre-fix downloader wrote to bare `XX/stem.ext`
        // at bucket root, invisible to `hs status`. Every new download
        // must now land under `papers/`.
        let d = mk("papers");
        assert_eq!(d.build_key("abcdef", "pdf"), "papers/ab/abcdef.pdf");
        assert_eq!(
            d.build_key("10.1007_s001", "html"),
            "papers/10/10.1007_s001.html"
        );
    }

    #[test]
    fn custom_papers_prefix_threads_through() {
        let d = mk("bulk-ingest");
        assert_eq!(d.build_key("xyz9", "pdf"), "bulk-ingest/xy/xyz9.pdf");
    }

    #[test]
    fn trailing_slash_on_prefix_is_trimmed() {
        // Operator typos in config shouldn't double-slash the key.
        let d = mk("papers/");
        assert_eq!(d.build_key("ab12", "pdf"), "papers/ab/ab12.pdf");
    }
}

#[cfg(test)]
mod arxiv_doi_tests {
    use super::strip_arxiv_doi_prefix;

    #[test]
    fn matches_mixed_case_arxiv() {
        assert_eq!(
            strip_arxiv_doi_prefix("10.48550/arXiv.2005.11401"),
            Some("2005.11401")
        );
        assert_eq!(
            strip_arxiv_doi_prefix("10.48550/arxiv.2312.10997"),
            Some("2312.10997")
        );
        assert_eq!(
            strip_arxiv_doi_prefix("10.48550/ARXIV.1706.03762"),
            Some("1706.03762")
        );
    }

    #[test]
    fn rejects_non_arxiv_doi() {
        assert_eq!(strip_arxiv_doi_prefix("10.1007/s11704-024-40231-1"), None);
        assert_eq!(strip_arxiv_doi_prefix("10.3390/ijms24031234"), None);
    }

    #[test]
    fn rejects_prefix_only() {
        assert_eq!(strip_arxiv_doi_prefix("10.48550/arxiv."), None);
        assert_eq!(strip_arxiv_doi_prefix("10.48550/arxiv"), None);
    }

    #[test]
    fn rejects_nearby_prefix() {
        assert_eq!(strip_arxiv_doi_prefix("10.48551/arxiv.1234"), None);
    }
}
