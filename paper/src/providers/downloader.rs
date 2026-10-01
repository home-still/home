use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use hs_common::event_bus::EventBus;
use hs_common::storage::Storage;
use reqwest::{header, Client};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::DownloadConfig;
use crate::error::{PaperError, SourceOutcome};
use crate::models::DownloadResult;
use crate::ports::download_service::DownloadService;
use crate::ports::provider::PaperProvider;
use crate::providers::url_guard::{self, UrlPolicy};
use crate::stem;

/// If `doi` is a DataCite-registered arXiv DOI of the form
/// `10.48550/arXiv.<id>` (any casing of `arXiv`), return the bare `<id>`.
/// Otherwise `None`. Used by both the download fast-path and the aggregate
/// `get_by_doi` router so an arXiv DOI always resolves to arXiv, not to the
/// other providers (which don't index this DOI prefix).
pub fn strip_arxiv_doi_prefix(doi: &str) -> Option<&str> {
    const PREFIX: &str = "10.48550/arxiv.";
    // `get` instead of `[..]`: `doi` is untrusted text, and a multi-byte
    // character straddling the prefix length makes the byte slice panic
    // (process abort under `panic = "abort"`). A non-boundary offset can
    // never be the ASCII prefix, so `None` is the correct answer.
    let head = doi.get(..PREFIX.len())?;
    if !head.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let id = doi.get(PREFIX.len()..)?;
    if id.is_empty() {
        return None;
    }
    Some(id)
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
}

#[derive(Deserialize)]
struct PmcIdConverterResponse {
    records: Vec<PmcIdRecord>,
}

#[derive(Deserialize)]
struct PmcIdRecord {
    pmcid: Option<String>,
}

/// The smallest valid PDF is ~70 bytes (`%PDF-1.x` + xref + trailer); 100 is
/// well below that. Catches 0-byte stubs (servers that return 200 with an
/// empty body) before they are stamped `downloaded: true` with the sha256 of
/// the empty string.
pub(crate) const MIN_PDF_BYTES: u64 = 100;

/// What a stored paper starts with. The scribe `%PDF` gate downstream is the
/// only acceptance test for conversion, so the download gate is at least as
/// strict: anything else is rejected before it reaches storage.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Never reserve more than this up front from a remote `Content-Length`; the
/// buffer grows with the bytes that actually arrive.
const PREALLOC_CAP: u64 = 1024 * 1024;

/// Candidate URLs tried per source. A record lists a handful; the cap bounds
/// the work a hostile record can ask for.
const MAX_CANDIDATE_URLS: usize = 8;

/// Bound on JSON bodies from the lookup APIs (Unpaywall, NCBI).
const MAX_API_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Base URLs of the fixed (non-provider) sources.
#[derive(Debug, Clone)]
struct Endpoints {
    arxiv: String,
    mdpi: String,
    unpaywall: String,
    ncbi_idconv: String,
    pmc: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            arxiv: "https://arxiv.org".to_string(),
            mdpi: "https://www.mdpi.com".to_string(),
            unpaywall: "https://api.unpaywall.org".to_string(),
            ncbi_idconv: "https://www.ncbi.nlm.nih.gov/pmc/utils/idconv/v1.0/".to_string(),
            pmc: "https://pmc.ncbi.nlm.nih.gov".to_string(),
        }
    }
}

/// A fetched, validated PDF body held in memory.
struct Fetched {
    bytes: Vec<u8>,
    sha256: String,
}

pub struct PaperDownloader {
    client: Client,
    policy: UrlPolicy,
    storage: Arc<dyn Storage>,
    events: Arc<dyn EventBus>,
    contact_email: Option<String>,
    /// Storage prefix all downloads land under (e.g. `"papers"`). The key
    /// for a downloaded artifact is `{papers_prefix}/{sharded_key(stem, ext)}`.
    /// Pre-rc.298 the prefix was silently omitted, scattering files across
    /// bucket-root shards — this field exists so writes and pipeline counts
    /// can't drift apart again.
    papers_prefix: String,
    max_bytes: u64,
    endpoints: Endpoints,
    resolvers: Vec<Arc<dyn PaperProvider>>,
}

impl PaperDownloader {
    pub fn with_event_bus(
        storage: Arc<dyn Storage>,
        events: Arc<dyn EventBus>,
        config: &DownloadConfig,
        resolvers: Vec<Arc<dyn PaperProvider>>,
    ) -> Result<Self, PaperError> {
        Self::build(
            storage,
            events,
            config,
            resolvers,
            UrlPolicy::public_only(),
            Endpoints::production(),
        )
    }

    fn build(
        storage: Arc<dyn Storage>,
        events: Arc<dyn EventBus>,
        config: &DownloadConfig,
        resolvers: Vec<Arc<dyn PaperProvider>>,
        policy: UrlPolicy,
        endpoints: Endpoints,
    ) -> Result<Self, PaperError> {
        config.validate()?;

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

        let client = policy
            .harden(
                Client::builder()
                    .timeout(std::time::Duration::from_secs(config.timeout_secs))
                    .user_agent(user_agent)
                    .default_headers(headers),
            )
            .build()?;

        Ok(Self {
            client,
            policy,
            storage,
            events,
            contact_email: config.unpaywall_email.clone(),
            papers_prefix: config.papers_prefix.clone(),
            max_bytes: config.max_download_bytes,
            endpoints,
            resolvers,
        })
    }

    /// Test-only: loopback allowed, every fixed source pointed at the fake
    /// server at `base` (`http://127.0.0.1:<port>`).
    #[cfg(test)]
    fn for_tests(
        storage: Arc<dyn Storage>,
        events: Arc<dyn EventBus>,
        config: &DownloadConfig,
        resolvers: Vec<Arc<dyn PaperProvider>>,
        base: &str,
    ) -> Self {
        let endpoints = Endpoints {
            arxiv: format!("{base}/arxiv"),
            mdpi: format!("{base}/mdpi"),
            unpaywall: format!("{base}/unpaywall"),
            ncbi_idconv: format!("{base}/idconv/"),
            pmc: format!("{base}/pmc"),
        };
        Self::build(
            storage,
            events,
            config,
            resolvers,
            UrlPolicy::allow_loopback_for_tests(),
            endpoints,
        )
        .expect("test downloader config is valid")
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

    /// `Some(skipped result)` when `key` is already stored.
    async fn existing(&self, key: &str) -> Result<Option<DownloadResult>, PaperError> {
        let meta = self
            .storage
            .head(key)
            .await
            .map_err(|e| PaperError::Storage(format!("head {key}: {e}")))?;
        Ok(meta.map(|meta| DownloadResult {
            file_path: PathBuf::from(key),
            doi: None,
            sha256: String::new(),
            size_bytes: meta.size,
            skipped: true,
        }))
    }

    /// GET `url` and return the validated PDF body.
    ///
    /// The body is collected in memory (storage `put` takes a `Vec`), so the
    /// stream is bounded: the transfer is aborted the moment `max_bytes` is
    /// crossed and a non-PDF is abandoned after its first chunk. A declared
    /// `Content-Length` only ever makes the refusal earlier; it never sizes
    /// an allocation beyond [`PREALLOC_CAP`].
    async fn fetch_pdf(
        &self,
        url: &Url,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<Fetched, PaperError> {
        let shown = url_guard::display_url(url.as_str());
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| url_guard::classify(&shown, e))?
            .error_for_status()
            .map_err(|e| url_guard::classify(&shown, e))?;

        let declared = response.content_length();
        if declared.is_some_and(|n| n > self.max_bytes) {
            return Err(PaperError::TooLarge {
                url: shown,
                limit: self.max_bytes,
            });
        }

        let prealloc = declared.map_or(0, |n| n.min(PREALLOC_CAP).min(self.max_bytes)) as usize;
        let mut hasher = Sha256::new();
        let mut checked_magic = false;
        let bytes = collect_body(
            response,
            &shown,
            self.max_bytes,
            prealloc,
            |chunk, so_far| {
                if !checked_magic && so_far.len() >= PDF_MAGIC.len() {
                    if !so_far.starts_with(PDF_MAGIC) {
                        return Err(not_a_pdf(&shown, so_far));
                    }
                    checked_magic = true;
                }
                hasher.update(chunk);
                if let Some(cb) = on_progress {
                    cb(so_far.len() as u64, declared);
                }
                Ok(())
            },
        )
        .await?;

        // Bodies shorter than the magic never reached the in-stream check.
        if !bytes.starts_with(PDF_MAGIC) {
            return Err(not_a_pdf(&shown, &bytes));
        }
        if (bytes.len() as u64) < MIN_PDF_BYTES {
            return Err(PaperError::NotPdf {
                url: shown,
                detail: format!(
                    "{} bytes (< {MIN_PDF_BYTES}); rejecting as a stub",
                    bytes.len()
                ),
            });
        }

        Ok(Fetched {
            sha256: format!("{:x}", hasher.finalize()),
            bytes,
        })
    }

    /// Write a fetched body, confirm it is queryable at the expected size,
    /// and announce it. Every failure here is local ([`PaperError::Storage`]).
    async fn store(&self, key: &str, fetched: Fetched) -> Result<DownloadResult, PaperError> {
        let size_bytes = fetched.bytes.len() as u64;
        let sha256 = fetched.sha256;
        self.storage
            .put(key, fetched.bytes)
            .await
            .map_err(|e| PaperError::Storage(format!("put {key}: {e}")))?;

        // Confirm the object is queryable + correctly sized before declaring
        // success. `put()` returning Ok is not always sufficient on
        // S3-compatible backends (Garage etc.); without this check, a
        // truncated or evicted object would still be stamped as
        // `downloaded: true` in the catalog and then explode at scribe time
        // with "No PDF or HTML found for <stem>".
        verify_put(self.storage.head(key).await, key, size_bytes)?;

        // Announce the new artifact so scribe (or any other subscriber) can
        // pick it up. On NoOpBus this is a cheap no-op; with NATS it reaches
        // every subscriber on `papers.ingested`.
        let payload = serde_json::json!({
            "key": key,
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
            tracing::warn!(key = %key, error = %e, "event publish failed");
        }

        Ok(DownloadResult {
            file_path: PathBuf::from(key),
            doi: None,
            sha256,
            size_bytes,
            skipped: false,
        })
    }

    /// Try each candidate URL of one source in order. `Ok(Some)` on the first
    /// PDF. A remote failure is recorded in `outcomes` and the next candidate
    /// is tried; a local failure (storage) aborts with the real error, since
    /// no other URL or source can fix a disk.
    async fn try_urls(
        &self,
        source: &str,
        urls: &[String],
        key: &str,
        outcomes: &mut Vec<SourceOutcome>,
    ) -> Result<Option<DownloadResult>, PaperError> {
        for raw in urls.iter().take(MAX_CANDIDATE_URLS) {
            let attempt = match self.policy.parse_and_check(raw) {
                Ok(url) => match self.fetch_pdf(&url, None).await {
                    Ok(fetched) => self.store(key, fetched).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            match attempt {
                Ok(result) => return Ok(Some(result)),
                Err(e) if e.is_local() => return Err(e),
                Err(e) => outcomes.push(outcome_for_url_error(
                    format!("{source} {}", url_guard::display_url(raw)),
                    &e,
                )),
            }
        }
        Ok(None)
    }

    /// GET a lookup API and parse its JSON. Every failure is a
    /// [`SourceOutcome`]: 404 means the source has no record, anything else
    /// means the source could not answer.
    async fn api_get_json<T: DeserializeOwned>(
        &self,
        source: &str,
        url: Url,
    ) -> Result<T, SourceOutcome> {
        let shown = url_guard::display_url(url.as_str());
        let response = self.client.get(url).send().await.map_err(|e| {
            SourceOutcome::failed(source, url_guard::classify(&shown, e).to_string())
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(SourceOutcome::no_copy(source, "no record (HTTP 404)"));
        }
        if !status.is_success() {
            return Err(SourceOutcome::failed(source, format!("HTTP {status}")));
        }
        let bytes = collect_body(response, &shown, MAX_API_RESPONSE_BYTES, 0, |_, _| Ok(()))
            .await
            .map_err(|e| SourceOutcome::failed(source, e.to_string()))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| SourceOutcome::failed(source, format!("unparseable response: {e}")))
    }

    /// Candidate PDF URLs from Unpaywall: the best location first, then every
    /// other location that links a PDF. Landing pages are not candidates —
    /// the download gate would reject them.
    async fn unpaywall_urls(&self, doi: &str) -> Result<Vec<String>, SourceOutcome> {
        const SOURCE: &str = "unpaywall";
        let Some(email) = self.contact_email.as_deref() else {
            return Err(SourceOutcome::no_copy(
                SOURCE,
                "not configured: set download.unpaywall_email to enable Unpaywall lookups",
            ));
        };
        let mut url = endpoint_url(
            &self.endpoints.unpaywall,
            std::iter::once("v2").chain(doi.split('/')),
        )
        .map_err(|e| SourceOutcome::failed(SOURCE, e.to_string()))?;
        url.query_pairs_mut().append_pair("email", email);
        tracing::debug!(doi, "Unpaywall lookup");

        let data: UnpaywallResponse = self.api_get_json(SOURCE, url).await?;
        if !data.is_oa {
            return Err(SourceOutcome::no_copy(SOURCE, "not open access"));
        }

        let mut urls: Vec<String> = Vec::new();
        let locations = data
            .best_oa_location
            .iter()
            .chain(data.oa_locations.iter().flatten());
        for pdf_url in locations.filter_map(|l| l.url_for_pdf.as_ref()) {
            if !urls.contains(pdf_url) {
                urls.push(pdf_url.clone());
            }
        }
        if urls.is_empty() {
            return Err(SourceOutcome::no_copy(
                SOURCE,
                "open access, but no location links a PDF",
            ));
        }
        Ok(urls)
    }

    /// PDF URL on PubMed Central, via NCBI's DOI → PMCID converter. NCBI asks
    /// (does not require) a contact `email`; it is sent when configured and
    /// omitted otherwise — never invented.
    async fn pmc_urls(&self, doi: &str) -> Result<Vec<String>, SourceOutcome> {
        const SOURCE: &str = "pmc";
        let mut url = Url::parse(&self.endpoints.ncbi_idconv)
            .map_err(|e| SourceOutcome::failed(SOURCE, format!("bad NCBI endpoint: {e}")))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("ids", doi)
                .append_pair("format", "json")
                .append_pair("tool", "home-still");
            if let Some(email) = &self.contact_email {
                query.append_pair("email", email);
            }
        }
        tracing::debug!(doi, "PMC ID Converter lookup");

        let data: PmcIdConverterResponse = self.api_get_json(SOURCE, url).await?;
        let Some(pmcid) = data.records.into_iter().find_map(|r| r.pmcid) else {
            return Err(SourceOutcome::no_copy(
                SOURCE,
                "DOI is not in PubMed Central",
            ));
        };
        // The PMCID comes from the response body and becomes a path segment.
        let digits = pmcid.strip_prefix("PMC").unwrap_or_default();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(SourceOutcome::failed(
                SOURCE,
                format!("NCBI returned an unexpected PMCID {pmcid:?}"),
            ));
        }
        let pdf = endpoint_url(&self.endpoints.pmc, ["articles", pmcid.as_str(), "pdf", ""])
            .map_err(|e| SourceOutcome::failed(SOURCE, e.to_string()))?;
        tracing::debug!(doi, pmcid = %pmcid, url = %pdf, "PMC direct PDF");
        Ok(vec![pdf.to_string()])
    }
}

#[async_trait]
impl DownloadService for PaperDownloader {
    /// Walk the ordered source chain — arXiv, MDPI, Unpaywall, PMC, then each
    /// configured provider — until one yields a PDF.
    ///
    /// A source that has no copy, or that fails over the network, is recorded
    /// and the next one is tried. A *local* failure (storage write, invalid
    /// key, I/O) aborts at once with the real error: it would not be fixed by
    /// another source, and reporting it as "no open-access PDF" would send
    /// the operator looking in the wrong place. When every source comes up
    /// empty the error lists each source's outcome.
    async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperError> {
        // One identity for one paper: DOIs are case-insensitive, so the stem
        // is derived from the lowercased bare DOI (see `crate::stem`).
        let doi = stem::normalize_doi(doi)?;
        let stem = stem::doi_stem(&doi)?;
        let key = self.build_key(&stem, "pdf");

        if let Some(existing) = self.existing(&key).await? {
            return Ok(existing);
        }

        let mut outcomes: Vec<SourceOutcome> = Vec::new();

        // 1. arXiv fast path — match the arXiv DOI prefix case-insensitively
        // (DataCite registration is `arXiv`, but consumers paste both
        // `arXiv` and `arxiv`; we should resolve either).
        if let Some(arxiv_id) = strip_arxiv_doi_prefix(&doi) {
            let url = arxiv_pdf_url(&self.endpoints.arxiv, arxiv_id)?;
            if let Some(hit) = self
                .try_urls("arxiv", &[url.to_string()], &key, &mut outcomes)
                .await?
            {
                return Ok(hit);
            }
        }

        // 1b. MDPI fast path — all MDPI journals are open access
        if doi
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("10.3390/"))
        {
            let url = endpoint_url(
                &self.endpoints.mdpi,
                doi.split('/').chain(std::iter::once("pdf")),
            )?;
            tracing::debug!(doi, url = %url, "MDPI direct PDF");
            if let Some(hit) = self
                .try_urls("mdpi", &[url.to_string()], &key, &mut outcomes)
                .await?
            {
                return Ok(hit);
            }
        }

        // 2. Unpaywall lookup
        match self.unpaywall_urls(&doi).await {
            Ok(urls) => {
                if let Some(hit) = self
                    .try_urls("unpaywall", &urls, &key, &mut outcomes)
                    .await?
                {
                    return Ok(hit);
                }
            }
            Err(outcome) => outcomes.push(outcome),
        }

        // 2b. PMC direct PDF (DOI → PMCID via NCBI ID Converter → PDF URL)
        match self.pmc_urls(&doi).await {
            Ok(urls) => {
                if let Some(hit) = self.try_urls("pmc", &urls, &key, &mut outcomes).await? {
                    return Ok(hit);
                }
            }
            Err(outcome) => outcomes.push(outcome),
        }

        // 3. Provider-based resolution (Semantic Scholar, Europe PMC, CORE, OpenAlex, CrossRef)
        for resolver in &self.resolvers {
            let name = resolver.name();
            match resolver.get_by_doi(&doi).await {
                Ok(Some(paper)) if !paper.download_urls.is_empty() => {
                    if let Some(hit) = self
                        .try_urls(name, &paper.download_urls, &key, &mut outcomes)
                        .await?
                    {
                        return Ok(hit);
                    }
                }
                Ok(Some(_)) => {
                    outcomes.push(SourceOutcome::no_copy(name, "record has no download URL"))
                }
                Ok(None) => outcomes.push(SourceOutcome::no_copy(name, "no record for this DOI")),
                Err(e) => outcomes.push(SourceOutcome::failed(name, e.to_string())),
            }
        }

        Err(PaperError::NoSourceYielded {
            doi,
            sources: outcomes,
        })
    }

    async fn download_by_url(
        &self,
        url: &str,
        stem: &str,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> Result<DownloadResult, PaperError> {
        stem::check_stem(stem)?;
        let url = self.policy.parse_and_check(url)?;
        let key = self.build_key(stem, "pdf");

        // Skip if already downloaded
        if let Some(existing) = self.existing(&key).await? {
            return Ok(existing);
        }

        let fetched = self.fetch_pdf(&url, on_progress).await?;
        self.store(&key, fetched).await
    }
}

/// Drain `response` into memory, refusing to hold more than `max` bytes.
/// `inspect(chunk, buffer_so_far)` runs after each chunk is appended and may
/// abort the transfer by returning `Err` (the connection is dropped with the
/// stream).
async fn collect_body<F>(
    response: reqwest::Response,
    shown: &str,
    max: u64,
    prealloc: usize,
    mut inspect: F,
) -> Result<Vec<u8>, PaperError>
where
    F: FnMut(&[u8], &[u8]) -> Result<(), PaperError>,
{
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(prealloc);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| url_guard::classify(shown, e))?;
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > max {
            return Err(PaperError::TooLarge {
                url: shown.to_string(),
                limit: max,
            });
        }
        buf.extend_from_slice(&chunk);
        inspect(&chunk, &buf)?;
    }
    Ok(buf)
}

fn not_a_pdf(url: &str, head: &[u8]) -> PaperError {
    let detail = if hs_common::html::looks_like_html(head) {
        "the server returned an HTML page (landing page, paywall or login), not a PDF".to_string()
    } else {
        let first: Vec<String> = head.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!(
            "body does not start with %PDF- (first bytes: {})",
            first.join(" ")
        )
    };
    PaperError::NotPdf {
        url: url.to_string(),
        detail,
    }
}

/// Classify a failed attempt on one candidate URL. The source answered with
/// something that is not a paper (landing page, paywall status, non-PDF) →
/// `NoCopy`; the request itself failed or was refused → `Failed`.
fn outcome_for_url_error(source: String, err: &PaperError) -> SourceOutcome {
    let no_copy = match err {
        PaperError::NotPdf { .. } | PaperError::NotFound(_) => true,
        PaperError::Http(e) => matches!(
            e.status().map(|s| s.as_u16()),
            Some(401 | 403 | 404 | 410 | 451)
        ),
        _ => false,
    };
    let detail = match err {
        PaperError::NotPdf { detail, .. } => detail.clone(),
        other => other.to_string(),
    };
    if no_copy {
        SourceOutcome::no_copy(source, detail)
    } else {
        SourceOutcome::failed(source, detail)
    }
}

/// `base` + path `segments`, each percent-encoded (`.`/`..` segments are
/// dropped by the `url` crate, never interpreted). A DOI's `/` separates
/// segments; any other reserved character inside one is encoded.
fn endpoint_url<'a>(
    base: &str,
    segments: impl IntoIterator<Item = &'a str>,
) -> Result<Url, PaperError> {
    let mut url = Url::parse(base)
        .map_err(|e| PaperError::InvalidInput(format!("bad endpoint {base:?}: {e}")))?;
    url.path_segments_mut()
        .map_err(|_| PaperError::InvalidInput(format!("endpoint {base:?} cannot be a base URL")))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// `https://arxiv.org/pdf/<id>` for a DOI-derived arXiv id. The id is part of
/// a DOI the caller typed; only the id shapes arXiv uses (`2005.11401`,
/// `2005.11401v2`, `hep-th/9901001`) are accepted.
fn arxiv_pdf_url(base: &str, id: &str) -> Result<Url, PaperError> {
    let segments: Vec<&str> = id.split('/').collect();
    let valid = segments.len() <= 2
        && segments.iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        });
    if !valid {
        return Err(PaperError::InvalidInput(format!(
            "{id:?} is not an arXiv identifier"
        )));
    }
    endpoint_url(base, std::iter::once("pdf").chain(segments))
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
        Ok(Some(meta)) => Err(PaperError::Storage(format!(
            "post-write verify: size mismatch for {key} (wrote {expected_size}, head reports {})",
            meta.size
        ))),
        Ok(None) => Err(PaperError::Storage(format!(
            "post-write verify: {key} not found after put"
        ))),
        Err(e) => Err(PaperError::Storage(format!(
            "post-write verify failed for {key}: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests;

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

    #[test]
    fn multibyte_text_around_the_prefix_boundary_is_not_an_arxiv_doi_and_never_panics() {
        // `"10.48550/arxiv."` is 15 bytes. These inputs put multi-byte chars
        // on or around byte 15 (the first, third and last straddle it), which
        // made `doi[..15]` abort the process (`panic = "abort"` in release)
        // on untrusted DOI text.
        for doi in [
            "10.48550/arxiv\u{00e9}2301.00001",
            "10.48550/arxi\u{00e9}.2301.00001",
            "10.48550/arxi\u{65e5}\u{672c}",
            "\u{65e5}\u{672c}\u{8a9e}\u{65e5}\u{672c}\u{8a9e}\u{65e5}\u{672c}\u{8a9e}",
            "10.48550/arxiv\u{1f600}",
        ] {
            assert_eq!(strip_arxiv_doi_prefix(doi), None, "input: {doi:?}");
        }
    }

    #[test]
    fn multibyte_id_after_a_valid_prefix_is_returned_verbatim() {
        // The prefix check only fixes the boundary at byte 15; what follows
        // is the caller's to validate.
        assert_eq!(
            strip_arxiv_doi_prefix("10.48550/arXiv.\u{00e9}1"),
            Some("\u{00e9}1")
        );
    }
}
