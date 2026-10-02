use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use hs_common::event_bus::{specs, EventBus};
use hs_common::storage::Storage;
use serde::{Deserialize, Serialize};

use crate::classify::{classify, ConvertFailure, FailureClass, FailureCode};
use crate::client::{compute_convert_timeout, ConversionResult, ScribeClient};
use crate::config::TimeoutPolicy;
use crate::epub::EpubLimits;

/// Handler outcome for the scribe consumer. `Permanent` → the message is
/// TERMed (JetStream will never redeliver); `Transient` → NAK'd with
/// backoff so a flaky cluster recovers without losing work. The split
/// exists because re-delivering a terminal failure (VLM repetition loop,
/// FormatError, paywall HTML, unsupported extension) just wastes GPU on
/// content that will never convert — while re-delivering a transient
/// failure (storage blip, scribe pool empty, network timeout) is the
/// whole reason we switched to JetStream.
pub enum HandlerError {
    Permanent(anyhow::Error),
    Transient(anyhow::Error),
}

impl HandlerError {
    fn as_error(&self) -> &anyhow::Error {
        match self {
            HandlerError::Permanent(e) | HandlerError::Transient(e) => e,
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandlerError::Permanent(e) => write!(f, "permanent: {e:#}"),
            HandlerError::Transient(e) => write!(f, "transient: {e:#}"),
        }
    }
}

impl std::fmt::Debug for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for HandlerError {}

const NAK_BACKOFF: Duration = Duration::from_secs(30);

/// Payload published by `paper` and any other ingestion source on
/// `papers.ingested`. Fields beyond `key` are optional — a minimal publisher
/// may only know the object key.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IngestedEvent {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Once-per-event source preparation: parse the key, check whether the
/// markdown already exists (idempotent retry), fetch the source bytes,
/// and — for PDFs — validate them and count pages for the page-scaled
/// timeout. Split out of [`convert_and_upload`] so the chain dispatcher
/// fetches and parses ONCE and every backend attempt reuses the same
/// bytes; escalation used to re-download and re-parse the whole book from
/// storage per backend.
pub enum SourcePrep {
    /// Markdown already present under this key — nothing to convert. The
    /// caller still owes distill the `scribe.completed` announcement: the
    /// attempt that wrote this markdown may have died before publishing it
    /// (see [`announce_completed`]).
    AlreadyConverted(String),
    Fetched(SourceObject),
}

/// The fetched source plus everything derivable from it that the chain
/// needs per attempt. Bytes are shared (`Bytes`) so per-backend dispatch
/// clones a refcount, not the file.
pub struct SourceObject {
    pub bytes: bytes::Bytes,
    pub stem: String,
    pub ext: String,
    /// PDF page count for the page-scaled timeout: `Some` for a PDF source
    /// (a PDF whose pages cannot be counted never gets this far), `None`
    /// for every other source type.
    pub pdf_pages: Option<u32>,
}

/// A failure of the event itself that no retry can fix.
fn permanent(code: FailureCode, message: impl Into<String>) -> HandlerError {
    HandlerError::Permanent(ConvertFailure::err(code, message))
}

/// Classify a storage error: a key that cannot name an object is the
/// event's fault and never worth redelivering; every other storage error
/// (network, 5xx, auth) is cluster state that may clear up.
fn storage_failure(err: anyhow::Error, context: String) -> HandlerError {
    if hs_common::storage::is_invalid_key(&err) {
        permanent(FailureCode::InvalidKey, format!("{context}: {err:#}"))
    } else {
        HandlerError::Transient(err.context(context))
    }
}

/// The document stem and lower-cased extension an event key designates, or
/// the reason the key cannot be converted. The key is untrusted (it arrives
/// on the bus): it must be a legal storage key and its stem a legal stem
/// before either is used to build another key.
fn parse_event_key(key: &str) -> Result<(String, String), HandlerError> {
    hs_common::storage::validate_key(key)
        .map_err(|e| permanent(FailureCode::InvalidKey, format!("event key rejected: {e}")))?;
    let filename = key.rsplit_once('/').map(|(_, f)| f).unwrap_or(key);
    let Some((stem, ext)) = filename.rsplit_once('.') else {
        return Err(permanent(
            FailureCode::MissingExtension,
            format!("key {key} has no extension"),
        ));
    };
    hs_common::validate_stem(stem).map_err(|e| {
        permanent(
            FailureCode::InvalidKey,
            format!("event key {key:?} has an unusable stem: {e}"),
        )
    })?;
    Ok((stem.to_string(), ext.to_ascii_lowercase()))
}

pub async fn prepare_source(
    storage: &dyn Storage,
    event: &IngestedEvent,
) -> Result<SourcePrep, HandlerError> {
    let (stem, ext) = parse_event_key(&event.key)?;
    let md_key = hs_common::markdown::markdown_storage_key(&stem);

    let exists = storage
        .exists(&md_key)
        .await
        .map_err(|e| storage_failure(e, format!("head({md_key}) failed")))?;
    if exists {
        tracing::info!(md_key = %md_key, "markdown already present; skipping");
        return Ok(SourcePrep::AlreadyConverted(md_key));
    }

    let raw_bytes = match storage.get(&event.key).await {
        Ok(b) => b,
        Err(e) => {
            // S3 NotFound (or LocalFs ErrorKind::NotFound) means the
            // source bytes don't exist. Re-delivering won't conjure
            // them — term the message and stamp `conversion_failed:
            // source_missing` on the catalog so source-scan skips it
            // on future reconciles. Other GET failures (network,
            // 5xx, auth) are transient cluster state.
            if hs_common::storage::is_not_found(&e) {
                if let Err(stamp_err) = hs_common::catalog::update_conversion_failed_via(
                    storage,
                    "catalog",
                    &stem,
                    FailureCode::SourceMissing.wire(),
                    Vec::new(),
                )
                .await
                {
                    tracing::error!(
                        stem = %stem,
                        error = %stamp_err,
                        "stamp source_missing failed",
                    );
                }
                return Err(permanent(
                    FailureCode::SourceMissing,
                    format!("source bytes missing for {}: {e:#}", event.key),
                ));
            }
            return Err(storage_failure(e, format!("get({}) failed", event.key)));
        }
    };
    let bytes = bytes::Bytes::from(raw_bytes);

    let pdf_pages = if ext == "pdf" {
        // pdfium reads the whole document (untrusted bytes, up to 256 MiB):
        // `count_pages` runs it on the blocking pool under a wall-clock
        // budget, from the shared `Bytes` (no byte copy). A failure that is
        // a verdict on the document is refused here with its cause —
        // never "unknown pages" dispatched under a fallback timeout —
        // while a failure of the host (libpdfium missing, the counter
        // busy behind a stuck parse) is retried.
        match crate::pdf_meta::count_pages(bytes.clone()).await {
            Ok(pages) => Some(pages),
            Err(e) if crate::classify::failure_code(&e).is_some() => {
                tracing::warn!(
                    key = %event.key,
                    code = crate::classify::failure_code(&e).map(|c| c.wire()),
                    error = %e,
                    "PDF refused before dispatch"
                );
                return Err(HandlerError::Permanent(
                    e.context(format!("{} cannot be converted", event.key)),
                ));
            }
            Err(e) => {
                return Err(HandlerError::Transient(
                    e.context(format!("counting the pages of {}", event.key)),
                ));
            }
        }
    } else {
        None
    };

    Ok(SourcePrep::Fetched(SourceObject {
        bytes,
        stem,
        ext,
        pdf_pages,
    }))
}

/// Tell distill that the markdown at `md_key` is ready by publishing
/// `scribe.completed`. A publish that fails is `Transient`: the event is
/// NAKed, and its redelivery lands on [`SourcePrep::AlreadyConverted`] —
/// whose caller announces again through here — so the announcement is
/// retried until it gets through instead of being lost behind an ACK.
/// Distill's handler is idempotent, so a repeat announcement only repeats
/// the index.
pub async fn announce_completed(
    bus: &dyn EventBus,
    md_key: &str,
    source_key: &str,
) -> Result<(), HandlerError> {
    let payload = serde_json::json!({
        "key": md_key,
        "source_key": source_key,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| {
        HandlerError::Permanent(anyhow::Error::new(e).context("encoding scribe.completed"))
    })?;
    bus.publish("scribe.completed", &bytes).await.map_err(|e| {
        HandlerError::Transient(e.context(format!("scribe.completed publish failed for {md_key}")))
    })
}

/// Everything the QC gate decided about a VLM conversion. Computed on the
/// blocking pool: `longest_repeated_run_bytes` and the repetition cleanup
/// are O(document) and allocate per character.
struct QcOutcome {
    markdown: String,
    verdict: crate::postprocess::QcVerdict,
    truncations: usize,
    longest_run: usize,
    skipped_regions: usize,
    total_pages: usize,
}

fn run_qc(conversion: ConversionResult, stem: &str) -> QcOutcome {
    let qc_started = std::time::Instant::now();
    let skipped_regions = conversion.skipped_regions();
    let ConversionResult {
        markdown,
        per_page_region_classes,
        per_page_diags,
    } = conversion;
    let longest_run = crate::postprocess::longest_repeated_run_bytes(&markdown);
    let (md_clean, per_page_truncations) =
        crate::postprocess::clean_repetitions_per_page(&markdown);
    let truncations: usize = per_page_truncations.iter().map(|t| t.total()).sum();
    // Align the bibliography flags with per_page_truncations.len().
    // The olmocr backend supplies an empty class vec; pad with
    // empty class lists so qc_verdict sees the same length on
    // both sides, all flagged as non-bibliography.
    let total_pages = per_page_truncations.len();
    let per_page_is_bibliography: Vec<bool> = (0..total_pages)
        .map(|i| {
            per_page_region_classes
                .get(i)
                .map(|classes| crate::postprocess::is_bibliography_page(classes))
                .unwrap_or(false)
        })
        .collect();
    let verdict = crate::postprocess::qc_verdict(
        &per_page_truncations,
        &per_page_is_bibliography,
        longest_run,
        skipped_regions,
    );
    // Optional --diag JSONL: opt-in via HS_SCRIBE_DIAG_DIR env var.
    // Server-side per-page records (collected during conversion)
    // and the document summary land in one append-only file per
    // stem for grep/jq inspection. Disabled in steady state via
    // the Option::is_none() check inside DiagWriter.
    let diag_dir = std::env::var_os("HS_SCRIBE_DIAG_DIR").map(std::path::PathBuf::from);
    let mut diag = crate::diag::DiagWriter::open(diag_dir.as_ref(), stem);
    for record in per_page_diags {
        diag.write_page(stem, record);
    }
    diag.write_document(crate::diag::DocSummaryRecord {
        stem: stem.to_string(),
        total_pages,
        per_page_truncation_counts: per_page_truncations,
        longest_run_bytes: longest_run,
        qc_verdict: format!("{verdict:?}"),
        wall_clock_ms: qc_started.elapsed().as_millis() as u64,
    });
    QcOutcome {
        markdown: md_clean,
        verdict,
        truncations,
        longest_run,
        skipped_regions,
        total_pages,
    }
}

/// Run a CPU-bound conversion step on the blocking pool. A panic inside it
/// is a deterministic fault of this document (permanent); losing the task
/// to runtime shutdown is not.
async fn blocking<T: Send + 'static>(
    what: &'static str,
    key: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, HandlerError> {
    tokio::task::spawn_blocking(f).await.map_err(|join| {
        if join.is_panic() {
            HandlerError::Permanent(anyhow::anyhow!("{what} panicked on {key}: {join}"))
        } else {
            HandlerError::Transient(anyhow::anyhow!(
                "{what} task for {key} was cancelled: {join}"
            ))
        }
    })
}

/// The wall-clock budget of an HTML or EPUB conversion is
/// `scribe.epub.html.max_convert_secs` (default 60 s). Expiry raises the
/// conversion's cancellation flag, which the HTML parser checks every 4 KiB,
/// so the blocking thread stops with the handler's wait instead of burning a
/// core for a document nobody waits for. The document is refused for good.
/// [`blocking`] under a wall-clock `budget`; running past it is a permanent
/// failure with `code`. The closure gets a cancellation flag which is raised
/// when the budget expires: a conversion that checks it (the HTML parser does,
/// between chunks) stops on its own, so the blocking thread does not keep
/// burning a core for a document nobody is waiting for.
async fn blocking_within<T: Send + 'static>(
    what: &'static str,
    key: &str,
    budget: Duration,
    code: FailureCode,
    f: impl FnOnce(&std::sync::atomic::AtomicBool) -> T + Send + 'static,
) -> Result<T, HandlerError> {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let for_task = Arc::clone(&cancel);
    match tokio::time::timeout(budget, blocking(what, key, move || f(&for_task))).await {
        Ok(result) => result,
        Err(_) => {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(permanent(
                code,
                format!("{what} of {key} did not finish within {budget:?}"),
            ))
        }
    }
}

/// Given a prepared source, dispatch to the converter for its file type
/// (PDF → scribe VLM, HTML → parser, EPUB → parser), and put the
/// markdown back under `markdown/{shard}/{stem}.md`. Publishes
/// `scribe.completed` with the markdown key on success.
///
/// Error classification (`classify.rs`, from typed [`FailureCode`]s — never
/// from message text):
///
/// - `Permanent` — content is unconvertable no matter how many times we
///   retry: unsupported extension, HTML not UTF-8, paywall/loading HTML,
///   EPUB parse failure, and scribe-side PDF parse errors. The scribe
///   server returns HTTP 415 with a `unsupported_content_type:{html,binary}`
///   body for bytes that fail the `%PDF` magic-byte gate; those bubble up
///   as Permanent too.
/// - `Transient` — cluster state that will recover: storage GET/PUT
///   failure, scribe 5xx / connection reset / dispatch timeout, no ready
///   scribe servers, a failed `scribe.completed` publish. The caller NAKs
///   with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn convert_and_upload(
    storage: &dyn Storage,
    scribe: &ScribeClient,
    bus: &dyn EventBus,
    event: &IngestedEvent,
    timeout_policy: &TimeoutPolicy,
    epub_limits: &EpubLimits,
    source: &SourceObject,
    // Step 2d chain context. `converted_by` identifies which backend in
    // `ScribeConfig.servers` ran this call so the catalog can record it;
    // `attempts_log` is the audit trail of any earlier backends that
    // escalated to this one. Both flow straight through to the success-
    // path catalog stamp; ignored on failure.
    converted_by: Option<String>,
    attempts_log: Vec<hs_common::catalog::AttemptEntry>,
) -> Result<String, HandlerError> {
    let stem = source.stem.as_str();
    let md_key = hs_common::markdown::markdown_storage_key(stem);

    let start = std::time::Instant::now();
    let (markdown, server) = match source.ext.as_str() {
        "pdf" => {
            // Size the per-request timeout by the page count prepared
            // once per event in `prepare_source`.
            let pages = source.pdf_pages.ok_or_else(|| {
                HandlerError::Permanent(anyhow::anyhow!(
                    "{} reached the PDF converter without a page count",
                    event.key
                ))
            })?;
            let timeout = compute_convert_timeout(pages, timeout_policy);
            tracing::info!(
                key = %event.key,
                pages,
                timeout_secs = timeout.as_secs(),
                "dispatching pdf to scribe with page-scaled timeout"
            );
            // Use the streaming endpoint so we get per-page PP-DocLayout-V3
            // region class names alongside the markdown — required for QC's
            // bibliography multiplier. Pass a no-op progress callback; this
            // handler has no UI to drive. The olmocr backend returns an
            // empty class vec; QC treats it as "not bibliography" → strict
            // default ceiling everywhere. The source bytes are shared with
            // the request body, not copied per attempt.
            let conversion = scribe
                .convert_with_progress(source.bytes.clone(), Some(timeout), Some(stem), |_| {})
                .await
                .map_err(|e| {
                    // Permanent/Escalate-class failures (broken PDF,
                    // VLM repetition loop, olmocr rejecting every page)
                    // must NOT NAK — retrying the same backend produces
                    // the same result, and poison-pill papers would
                    // redeliver every 30 s on JetStream and hog every
                    // in-flight slot. They surface as
                    // HandlerError::Permanent so the chain dispatcher
                    // can consult `classify::classify` for the
                    // stop-vs-next-backend decision. Only
                    // `FailureClass::Transient` (cluster-state problems,
                    // and any failure the server did not type) NAKs. One
                    // table decides: `classify.rs`.
                    let class = classify(&e);
                    let ctx = e.context(format!("scribe convert failed for {}", event.key));
                    match class {
                        FailureClass::Transient => HandlerError::Transient(ctx),
                        _ => HandlerError::Permanent(ctx),
                    }
                })?;
            // VLM-only QC. HTML/EPUB parsers don't repeat tokens, and
            // their natural structural repetition (headings, tables)
            // trips clean_repetitions and explodes into a retry storm.
            //
            // Reject-on-loop terminally stamps `conversion_failed` so the
            // source-scan in `hs-common/src/status.rs:856` skips it on
            // future passes — the prior attempt at rejection (disabled
            // 2026-04-23) didn't write the terminal stamp and the source
            // got re-queued forever. Operators retry via `hs scribe
            // reconvert`, which clears the stamp and republishes once.
            let stem_for_qc = stem.to_string();
            let qc = blocking("VLM output QC", &event.key, move || {
                run_qc(conversion, &stem_for_qc)
            })
            .await?;
            let reject = match qc.verdict {
                crate::postprocess::QcVerdict::Accept => None,
                crate::postprocess::QcVerdict::RejectLoop => Some(ConvertFailure::new(
                    FailureCode::VlmRepetitionLoop,
                    format!(
                        "VLM repetition loop on {} (truncations={}, longest_run={}B)",
                        event.key, qc.truncations, qc.longest_run
                    ),
                )),
                crate::postprocess::QcVerdict::RejectGapped => Some(ConvertFailure::new(
                    FailureCode::GappedConversion,
                    format!(
                        "{} converted with {} region(s) the server could not process; \
                         refusing to record a conversion with holes",
                        event.key, qc.skipped_regions
                    ),
                )),
            };
            if let Some(failure) = reject {
                if let Err(e) = hs_common::catalog::update_conversion_failed_via(
                    storage,
                    "catalog",
                    stem,
                    failure.code().wire(),
                    Vec::new(),
                )
                .await
                {
                    tracing::error!(
                        stem = %stem,
                        error = %e,
                        "stamp conversion_failed failed",
                    );
                }
                return Err(HandlerError::Permanent(anyhow::Error::new(failure)));
            }
            if qc.truncations > 0 {
                tracing::info!(
                    stem = %stem,
                    truncations = qc.truncations,
                    longest_run = qc.longest_run,
                    pages = qc.total_pages,
                    "cleaned VLM repetition site(s)",
                );
            }
            (qc.markdown, "scribe-vlm")
        }
        "html" | "htm" => {
            let bytes = source.bytes.clone();
            let key = event.key.clone();
            let limits = epub_limits.clone();
            let converted = blocking_within(
                "HTML conversion",
                &event.key,
                Duration::from_secs(limits.html.max_convert_secs),
                FailureCode::HtmlParseError,
                move |cancel| {
                    let html = std::str::from_utf8(&bytes).map_err(|e| {
                        ConvertFailure::new(
                            FailureCode::HtmlNotUtf8,
                            format!("HTML at {key} is not valid UTF-8: {e}"),
                        )
                    })?;
                    // Reject paywall / loading-stub / landing-page HTML before we
                    // spend time extracting markdown that would just be stamped
                    // `embedding_skip: zero_chunks_or_empty` downstream. Mirrors
                    // the check the downloader runs at ingress — putting it here
                    // too catches HTMLs that entered via any other path
                    // (scribe_inbox, bulk import, etc.).
                    if hs_common::html::is_paywall_html(html) {
                        return Err(ConvertFailure::new(
                            FailureCode::PaywallHtml,
                            format!(
                                "{key} looks like a paywall/loading-stub HTML; refusing to convert"
                            ),
                        ));
                    }
                    crate::html::convert_html_to_markdown(html, &limits.html, cancel)
                },
            )
            .await?;
            (
                converted.map_err(|f| HandlerError::Permanent(anyhow::Error::new(f)))?,
                "html-parser",
            )
        }
        "epub" => {
            let bytes = source.bytes.clone();
            let limits = epub_limits.clone();
            let converted = blocking_within(
                "EPUB conversion",
                &event.key,
                Duration::from_secs(limits.html.max_convert_secs),
                FailureCode::EpubParseError,
                move |cancel| crate::epub::convert_epub_to_markdown_with(&bytes, &limits, cancel),
            )
            .await?;
            let md = converted.map_err(|e| {
                permanent(
                    FailureCode::EpubParseError,
                    format!("EPUB parse failed for {}: {e:#}", event.key),
                )
            })?;
            (md, "epub-parser")
        }
        other => {
            return Err(permanent(
                FailureCode::UnsupportedExtension,
                format!(
                    "unsupported source type `.{other}` for {} — supported: .pdf, .html, .htm, .epub",
                    event.key
                ),
            ));
        }
    };
    let duration_secs = start.elapsed().as_secs_f64();

    // A conversion that produced nothing embeddable is a failed conversion,
    // not a successful one. Without this gate the stub was written to
    // storage, stamped `conversion` success and published as
    // `scribe.completed`; distill then filtered every chunk under the same
    // floor and recorded `embedding_skip: zero_chunks_or_empty` — leaving a
    // catalog row claiming success for a document that never had content.
    // Observed shape: a Radware 302 anti-bot page that reached the
    // html-parser as `# 302 Found\n\nrdwr` (17 bytes).
    //
    // Parser output under the floor is content-intrinsic (every tier runs
    // the same parser); VLM output under the floor is not — a different VLM
    // may read a scan the first could not — so the two are different codes.
    if !hs_common::quality::has_indexable_content(&markdown) {
        let code = if server == "scribe-vlm" {
            FailureCode::EmptyVlmConversion
        } else {
            FailureCode::EmptyConversion
        };
        if let Err(e) = hs_common::catalog::update_conversion_failed_via(
            storage,
            "catalog",
            stem,
            code.wire(),
            Vec::new(),
        )
        .await
        {
            tracing::error!(stem = %stem, error = %e, "stamp conversion_failed failed");
        }
        return Err(permanent(
            code,
            format!(
                "{} converted by {} to {} non-whitespace chars, below the {}-char indexable floor; \
                 refusing to record a conversion",
                event.key,
                server,
                hs_common::quality::non_whitespace_len(&markdown),
                hs_common::quality::MIN_INDEXABLE_NON_WS,
            ),
        ));
    }

    // Two independent page signals: the separator structure the backend
    // emitted, and the source's own page count parsed at ingest. See
    // `resolve_page_accounting` for why markdown structure wins when it
    // exists and why a lone offset over a multi-page source is dropped.
    let page_offsets = hs_common::catalog::compute_page_offsets(&markdown);
    let accounting =
        hs_common::catalog::resolve_page_accounting(page_offsets.len() as u64, source.pdf_pages);
    let total_pages = accounting.total_pages;
    let page_offsets = if accounting.offsets_trustworthy {
        page_offsets
    } else {
        Vec::new()
    };

    storage
        .put(&md_key, markdown.into_bytes())
        .await
        .map_err(|e| storage_failure(e, format!("put({md_key}) failed")))?;

    // Stamp the catalog with the converter used so downstream can tell
    // which pipeline produced this markdown without guessing. The markdown
    // is already committed to storage, and a redelivery would find it
    // (`AlreadyConverted`) rather than re-run the VLM, so a lost stamp
    // cannot be retried through the event: it is logged as an ERROR and the
    // event still completes, so distill is not held back by bookkeeping.
    // `catalog_repair` reconciles markdown that has no conversion row.
    if let Err(e) = hs_common::catalog::update_conversion_catalog_via(
        storage,
        "catalog",
        stem,
        server,
        duration_secs,
        total_pages,
        page_offsets,
        &md_key,
        converted_by,
        attempts_log,
    )
    .await
    {
        tracing::error!(
            stem = %stem,
            md_key = %md_key,
            error = %e,
            "conversion catalog stamp failed — markdown is stored without a conversion row; \
             `catalog_repair` will report it as disk_no_catalog"
        );
    }

    announce_completed(bus, &md_key, &event.key).await?;

    Ok(md_key)
}

/// Pull-consume `papers.ingested` from JetStream and dispatch each event
/// to `handler`. Uses a durable JetStream consumer (see `specs::PAPERS_
/// INGESTED`) so messages survive subscriber restarts — unlike the core
/// NATS queue-subscribe that preceded it, which lost in-flight work on
/// watcher restart (rc.278 post-mortem: ~2,156 papers silently dropped).
///
/// Ack policy:
/// - `Ok(())` → `ack`. Message retired.
/// - `Err(Permanent)` → `term`. Message never retried.
/// - `Err(Transient)` → `nak` with `NAK_BACKOFF`. JetStream re-delivers.
/// - The handler **panicked** → `term`, logged at ERROR with the event key.
///   A panic is a bug in the code, triggered by this input: redelivery would
///   run the same code on the same bytes (repeating the conversion work up
///   to `max_deliver` times, each attempt `ack_wait` away) and panic again,
///   so the event is retired at once and the document is picked up again by
///   the catch-up sweep once the bug is fixed. Without the guard the panic
///   killed the task that owned the event, which then sat un-acked until
///   `ack_wait` (2 h by default) before the same panic repeated.
///
/// `max_deliver` on the consumer spec bounds total redeliveries, so a
/// stuck-in-transient-loop message eventually surfaces as a permanent
/// failure in operator logs.
///
/// This function returns only with an error. The message stream ends (or
/// yields a delivery error) when the broker drops the consumer or the
/// connection (a competing watcher deleting the durable, a broker restart,
/// missed heartbeats); consumption has then stopped, so returning `Ok` would
/// let the process exit 0 and stay down. Handlers already running get
/// `drain_timeout` to finish their ack/nak; the ones still running after it
/// are abandoned, named in an ERROR log, and redelivered after `ack_wait`.
/// Start-up check of the shared backend token: one authenticated request per
/// scribe server *before* any event is pulled. A server that rejects the
/// token stops the watcher here, naming `HS_BACKEND_TOKEN`; a server that
/// cannot be reached (or answers anything else) says nothing about the
/// credential and is only logged, because the watcher is meant to wait out a
/// server that is still starting.
pub async fn preflight_token(servers: &[String], convert_timeout: Duration) -> Result<()> {
    for url in servers {
        let client = crate::client::ScribeClient::new_with_timeout(url, convert_timeout)?;
        match client.preflight().await {
            Ok(()) => tracing::info!(server = %url, "scribe token preflight ok"),
            Err(e) if crate::client::is_backend_unauthorized(&e) => {
                tracing::error!(
                    server = %url,
                    "FATAL CONFIGURATION: the scribe server rejected HS_BACKEND_TOKEN at start-up; \
                     no event was pulled"
                );
                return Err(e);
            }
            Err(e) => tracing::warn!(
                server = %url,
                error = %format!("{e:#}"),
                "scribe token preflight could not reach the server; continuing"
            ),
        }
    }
    Ok(())
}

/// Most buffered events handed back on shutdown, and the longest the
/// hand-back may take.
const MAX_UNSTARTED_NAKS: usize = 256;
const UNSTARTED_NAK_BUDGET: Duration = Duration::from_secs(5);

/// NAK (immediate redelivery) `held` plus whatever the consumer has already
/// buffered for this process, bounded in count and time, and log the number.
async fn nak_unstarted(
    stream: &mut hs_common::event_bus::EventStream,
    mut held: Vec<hs_common::event_bus::Event>,
) {
    let deadline = tokio::time::Instant::now() + UNSTARTED_NAK_BUDGET;
    while held.len() < MAX_UNSTARTED_NAKS {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait = left.min(Duration::from_millis(200));
        if wait.is_zero() {
            break;
        }
        match tokio::time::timeout(wait, stream.next()).await {
            Ok(Some(Ok(event))) => held.push(event),
            _ => break,
        }
    }
    if held.is_empty() {
        return;
    }
    let total = held.len();
    let mut failed = 0usize;
    for event in held {
        if tokio::time::Instant::now() >= deadline || event.nak(None).await.is_err() {
            failed += 1;
        }
    }
    tracing::warn!(
        returned = total - failed,
        not_returned = failed,
        "shutting down: events pulled but never started were handed back for immediate \
         redelivery (any not returned reappear after ack_wait)"
    );
}

pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    concurrency: usize,
    drain_timeout: Duration,
    handler: F,
) -> Result<()>
where
    F: Fn(IngestedEvent) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), HandlerError>> + Send + 'static,
{
    let mut stream = bus.consume(&specs::PAPERS_INGESTED).await?;
    let concurrency = concurrency.max(1);
    tracing::info!(
        concurrency,
        "scribe consuming papers.ingested (durable: {})",
        specs::PAPERS_INGESTED.durable_name
    );

    // Cap in-flight dispatches so the watcher doesn't spawn thousands of
    // tasks at once. The loop naturally blocks on `acquire_owned` when
    // all slots are busy, which gives JetStream back-pressure for free.
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let handler = Arc::new(handler);
    let in_flight = hs_common::event_bus::InFlight::default();

    let mut delivery_error = None;
    // A handler that meets a fatal configuration error (the backend refuses
    // our token) reports it here; the loop stops consuming at once.
    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Error>();
    let mut fatal: Option<anyhow::Error> = None;
    // Events pulled from the broker whose handler never started.
    let mut unstarted: Vec<hs_common::event_bus::Event> = Vec::new();
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            Some(e) = fatal_rx.recv() => {
                fatal = Some(e);
                break;
            }
        };
        let Some(next) = next else { break };
        let event = match next {
            Ok(event) => event,
            Err(e) => {
                delivery_error = Some(e);
                break;
            }
        };
        let parsed: IngestedEvent = match serde_json::from_slice(&event.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    payload_len = event.payload.len(),
                    "malformed papers.ingested payload — terminating (will not redeliver)"
                );
                if let Err(term_err) = event.term().await {
                    tracing::warn!(error = %term_err, "failed to term malformed event");
                }
                continue;
            }
        };

        // Waiting for a slot is also a point where a fatal error must be
        // seen: the event in hand has not started, and goes back unstarted.
        let permit = tokio::select! {
            biased;
            Some(e) = fatal_rx.recv() => {
                fatal = Some(e);
                unstarted.push(event);
                break;
            }
            permit = sem.clone().acquire_owned() => {
                permit.map_err(|_| anyhow::anyhow!("scribe worker semaphore closed"))?
            }
        };
        let handler = Arc::clone(&handler);
        let fatal_tx = fatal_tx.clone();
        let tracked = in_flight.track(parsed.key.clone());
        tokio::spawn(async move {
            let _permit = permit; // drop at scope end releases the slot
            let _tracked = tracked;
            let key = parsed.key.clone();
            tracing::info!(key = %key, "scribe received ingested event");
            // The guard wraps the call too: a panic before the handler's
            // first await would otherwise escape it.
            let result =
                match hs_common::panic_guard::catch_panic(async move { handler(parsed).await })
                    .await
                {
                    Ok(result) => result,
                    Err(panic) => {
                        tracing::error!(
                            key = %key,
                            panic = %panic,
                            "scribe handler PANICKED — terminating this event (will not redeliver)"
                        );
                        if let Err(e) = event.term().await {
                            tracing::warn!(key = %key, error = %e, "term after panic failed");
                        }
                        return;
                    }
                };
            match result {
                Ok(()) => {
                    if let Err(e) = event.ack().await {
                        tracing::warn!(key = %key, error = %e, "ack failed");
                    }
                }
                Err(err) if crate::client::is_backend_unauthorized(err.as_error()) => {
                    // Not a verdict on the document, and retrying cannot
                    // help: give the event back untouched, then stop the
                    // whole watcher loudly so the supervisor restarts it.
                    tracing::error!(
                        key = %key,
                        error = %err.as_error(),
                        "FATAL CONFIGURATION: the scribe server rejected HS_BACKEND_TOKEN; \
                         the watcher is stopping"
                    );
                    if let Err(e) = event.nak(Some(NAK_BACKOFF)).await {
                        tracing::warn!(key = %key, error = %e, "nak failed");
                    }
                    let _ = fatal_tx.send(anyhow::anyhow!(
                        "HS_BACKEND_TOKEN rejected by the scribe server: {}",
                        err.as_error()
                    ));
                }
                Err(err) => {
                    let is_perm = matches!(err, HandlerError::Permanent(_));
                    let inner = err.as_error();
                    if is_perm {
                        tracing::error!(
                            key = %key,
                            error = ?inner,
                            "scribe handler permanent failure — terminating (will not redeliver)"
                        );
                        if let Err(e) = event.term().await {
                            tracing::warn!(key = %key, error = %e, "term failed");
                        }
                    } else {
                        tracing::warn!(
                            key = %key,
                            error = ?inner,
                            backoff_secs = NAK_BACKOFF.as_secs(),
                            "scribe handler transient failure — redelivering after backoff"
                        );
                        if let Err(e) = event.nak(Some(NAK_BACKOFF)).await {
                            tracing::warn!(key = %key, error = %e, "nak failed");
                        }
                    }
                }
            }
        });
    }

    // Give back what was pulled and never started, so the broker redelivers
    // it now instead of after `ack_wait` (hours). Only an orderly exit can do
    // this: a crash or the pdfium exit-70 leaves such events invisible until
    // `ack_wait` expires.
    nak_unstarted(&mut stream, unstarted).await;

    // Let handlers that are already running finish their ack/nak, within
    // the drain timeout.
    let abandoned =
        hs_common::event_bus::drain_in_flight(&sem, concurrency, drain_timeout, &in_flight).await;
    let abandoned_note = if abandoned.is_empty() {
        String::new()
    } else {
        format!(
            " ({} handler(s) abandoned after the drain timeout)",
            abandoned.len()
        )
    };
    if let Some(e) = fatal {
        return Err(e);
    }
    // A fatal error raised by a handler that was still running is fatal too.
    if let Ok(e) = fatal_rx.try_recv() {
        return Err(e);
    }
    Err(match delivery_error {
        Some(e) => e.context(format!(
            "event delivery for {} failed: the consumer is no longer receiving{abandoned_note}",
            specs::PAPERS_INGESTED.subject
        )),
        None => anyhow::anyhow!(
            "event stream ended: the consumer or broker connection for {} is gone{abandoned_note}",
            specs::PAPERS_INGESTED.subject
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{failure_code, FailureClass};
    use crate::pdf_meta::tests::{pdf_with_pages, skip_without_pdfium};
    use hs_common::event_bus::{ConsumerSpec, Event, EventStream};
    use hs_common::storage::LocalFsStorage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeBus {
        published: Mutex<Vec<(String, serde_json::Value)>>,
        fail_next_publishes: AtomicUsize,
        to_consume: Mutex<Vec<Event>>,
        /// When set, the stream yields this delivery error after the events.
        then_fail: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl EventBus for FakeBus {
        async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
            if self
                .fail_next_publishes
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                anyhow::bail!("broker unreachable");
            }
            self.published
                .lock()
                .unwrap()
                .push((subject.to_string(), serde_json::from_slice(payload)?));
            Ok(())
        }
        async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
            let events = std::mem::take(&mut *self.to_consume.lock().unwrap());
            let failure = self.then_fail.lock().unwrap().take();
            let items = events
                .into_iter()
                .map(Ok)
                .chain(failure.map(|m| Err(anyhow::anyhow!(m))));
            Ok(Box::pin(futures_util::stream::iter(items)))
        }
    }

    fn event(key: &str) -> IngestedEvent {
        IngestedEvent {
            key: key.into(),
            sha256: None,
            size_bytes: None,
            source: None,
        }
    }

    fn code_of(err: &HandlerError) -> (&'static str, Option<FailureCode>) {
        match err {
            HandlerError::Permanent(e) => ("permanent", failure_code(e)),
            HandlerError::Transient(e) => ("transient", failure_code(e)),
        }
    }

    fn storage() -> (tempfile::TempDir, LocalFsStorage) {
        let dir = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(dir.path());
        (dir, s)
    }

    #[tokio::test]
    async fn keys_that_cannot_name_a_document_are_permanent_and_touch_no_storage() {
        let (_d, st) = storage();
        for (key, code) in [
            ("../../etc/passwd.pdf", FailureCode::InvalidKey),
            ("/abs/doc.pdf", FailureCode::InvalidKey),
            ("papers/ab/..pdf", FailureCode::InvalidKey),
            ("papers/ab/.pdf", FailureCode::InvalidKey),
            ("papers\\ab\\doc.pdf", FailureCode::InvalidKey),
            ("papers/ab/noext", FailureCode::MissingExtension),
        ] {
            let err = prepare_source(&st, &event(key)).await.err().expect(key);
            assert_eq!(code_of(&err), ("permanent", Some(code)), "{key}");
        }
    }

    #[tokio::test]
    async fn a_missing_source_is_permanent_and_stamped() {
        let (_d, st) = storage();
        let err = prepare_source(&st, &event("papers/ab/absent.pdf"))
            .await
            .err()
            .unwrap();
        assert_eq!(
            code_of(&err),
            ("permanent", Some(FailureCode::SourceMissing))
        );
        let row = hs_common::catalog::read_catalog_entry_via(&st, "catalog", "absent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.conversion_failed.unwrap().reason, "source_missing");
    }

    #[tokio::test]
    async fn html_named_pdf_and_broken_pdfs_are_refused_before_any_dispatch() {
        let (_d, st) = storage();
        st.put(
            "papers/ab/paywall.pdf",
            b"<!DOCTYPE html><html>log in</html>".to_vec(),
        )
        .await
        .unwrap();
        st.put("papers/ab/broken.pdf", b"%PDF-1.4\nnot really".to_vec())
            .await
            .unwrap();
        let html = prepare_source(&st, &event("papers/ab/paywall.pdf"))
            .await
            .err()
            .unwrap();
        assert_eq!(
            code_of(&html),
            ("permanent", Some(FailureCode::UnsupportedContentTypeHtml))
        );
        // The structural verdict below is pdfium's.
        skip_without_pdfium!("html_named_pdf_and_broken_pdfs: broken-PDF half");
        let broken = prepare_source(&st, &event("papers/ab/broken.pdf"))
            .await
            .err()
            .unwrap();
        assert_eq!(
            code_of(&broken),
            ("permanent", Some(FailureCode::PdfParseError))
        );
    }

    #[tokio::test]
    async fn a_valid_pdf_is_fetched_with_its_page_count() {
        skip_without_pdfium!("a_valid_pdf_is_fetched_with_its_page_count");
        let (_d, st) = storage();
        st.put("papers/ab/ok.pdf", pdf_with_pages(3)).await.unwrap();
        match prepare_source(&st, &event("papers/ab/ok.pdf")).await {
            Ok(SourcePrep::Fetched(src)) => {
                assert_eq!(src.pdf_pages, Some(3));
                assert_eq!(src.stem, "ok");
            }
            _ => panic!("expected a fetched source"),
        }
    }

    #[tokio::test]
    async fn existing_markdown_is_already_converted_and_can_be_announced() {
        let (_d, st) = storage();
        let md_key = hs_common::markdown::markdown_storage_key("done");
        st.put(&md_key, b"# text".to_vec()).await.unwrap();
        let prep = prepare_source(&st, &event("papers/ab/done.pdf")).await;
        let Ok(SourcePrep::AlreadyConverted(key)) = prep else {
            panic!("expected AlreadyConverted");
        };
        assert_eq!(key, md_key);

        let bus = FakeBus::default();
        announce_completed(&bus, &key, "papers/ab/done.pdf")
            .await
            .unwrap();
        let published = bus.published.lock().unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "scribe.completed");
        assert_eq!(published[0].1["key"], md_key);
        assert_eq!(published[0].1["source_key"], "papers/ab/done.pdf");
    }

    #[tokio::test]
    async fn a_failed_completed_publish_is_transient_and_the_retry_goes_through() {
        let bus = FakeBus::default();
        bus.fail_next_publishes.store(1, Ordering::SeqCst);
        let err = announce_completed(&bus, "markdown/ab/x.md", "papers/ab/x.pdf")
            .await
            .unwrap_err();
        assert!(matches!(err, HandlerError::Transient(_)));
        assert!(bus.published.lock().unwrap().is_empty());
        announce_completed(&bus, "markdown/ab/x.md", "papers/ab/x.pdf")
            .await
            .unwrap();
        assert_eq!(bus.published.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stem_that_names_a_verdict_is_still_transient_when_the_server_is_down() {
        // The old substring table permanently failed any key containing
        // "paywall". A refused connection says nothing about the document.
        skip_without_pdfium!("a_stem_that_names_a_verdict_is_still_transient");
        let (_d, st) = storage();
        st.put("papers/pa/paywall-economics.pdf", pdf_with_pages(2))
            .await
            .unwrap();
        let Ok(SourcePrep::Fetched(src)) =
            prepare_source(&st, &event("papers/pa/paywall-economics.pdf")).await
        else {
            panic!("fetch");
        };
        let client =
            ScribeClient::new_with_timeout("http://127.0.0.1:1", Duration::from_secs(5)).unwrap();
        let bus = FakeBus::default();
        let err = convert_and_upload(
            &st,
            &client,
            &bus,
            &event("papers/pa/paywall-economics.pdf"),
            &TimeoutPolicy::default(),
            &EpubLimits::default(),
            &src,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(code_of(&err), ("transient", None), "{err}");
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stub_html_page_is_permanent_and_nothing_is_stored_or_announced() {
        let (_d, st) = storage();
        st.put(
            "papers/ab/stub.html",
            b"<html><body><p>hi</p></body></html>".to_vec(),
        )
        .await
        .unwrap();
        let Ok(SourcePrep::Fetched(src)) = prepare_source(&st, &event("papers/ab/stub.html")).await
        else {
            panic!("fetch");
        };
        let client =
            ScribeClient::new_with_timeout("http://127.0.0.1:1", Duration::from_secs(5)).unwrap();
        let bus = FakeBus::default();
        let err = convert_and_upload(
            &st,
            &client,
            &bus,
            &event("papers/ab/stub.html"),
            &TimeoutPolicy::default(),
            &EpubLimits::default(),
            &src,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();
        // Whichever stub verdict the HTML gates reach (paywall heuristic or
        // the indexable floor), it is permanent and typed.
        assert!(
            matches!(
                code_of(&err),
                (
                    "permanent",
                    Some(FailureCode::PaywallHtml | FailureCode::EmptyConversion)
                )
            ),
            "{err}"
        );
        assert_eq!(classify(&anyhow::anyhow!("x")), FailureClass::Transient);
        assert!(!st
            .exists(&hs_common::markdown::markdown_storage_key("stub"))
            .await
            .unwrap());
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn html_nested_past_the_bound_is_permanent_and_nothing_is_stored_or_announced() {
        // 20 000 nested <div>s aborted a 2 MiB-stack thread in the recursive
        // walker; the conversion now refuses the document before parsing it.
        let (_d, st) = storage();
        let prose = "A real paragraph of article text that is long enough to look like content. "
            .repeat(80);
        let deep = format!(
            "<html><body><article><p>{prose}</p>{}{prose}{}</article></body></html>",
            "<div>".repeat(20_000),
            "</div>".repeat(20_000)
        );
        st.put("papers/ab/deep.html", deep.into_bytes())
            .await
            .unwrap();
        let Ok(SourcePrep::Fetched(src)) = prepare_source(&st, &event("papers/ab/deep.html")).await
        else {
            panic!("fetch");
        };
        let client =
            ScribeClient::new_with_timeout("http://127.0.0.1:1", Duration::from_secs(5)).unwrap();
        let bus = FakeBus::default();
        let err = convert_and_upload(
            &st,
            &client,
            &bus,
            &event("papers/ab/deep.html"),
            &TimeoutPolicy::default(),
            &EpubLimits::default(),
            &src,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            code_of(&err),
            ("permanent", Some(FailureCode::HtmlParseError)),
            "{err}"
        );
        assert!(!st
            .exists(&hs_common::markdown::markdown_storage_key("deep"))
            .await
            .unwrap());
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_conversion_past_its_wall_clock_budget_is_refused_for_good() {
        let started = std::time::Instant::now();
        let err = blocking_within(
            "HTML conversion",
            "papers/ab/slow.html",
            Duration::from_millis(50),
            FailureCode::HtmlParseError,
            |_| std::thread::sleep(Duration::from_millis(1500)),
        )
        .await
        .unwrap_err();
        assert_eq!(
            code_of(&err),
            ("permanent", Some(FailureCode::HtmlParseError)),
            "{err}"
        );
        // The handler stopped waiting at the budget, not when the thread did.
        assert!(started.elapsed() < Duration::from_millis(1000));
        // Within budget, the result comes through untouched.
        let ok = blocking_within(
            "HTML conversion",
            "papers/ab/fast.html",
            Duration::from_secs(5),
            FailureCode::HtmlParseError,
            |_| 7,
        )
        .await
        .unwrap();
        assert_eq!(ok, 7);
    }

    async fn subscribe(
        events: Vec<Event>,
        concurrency: usize,
    ) -> (anyhow::Result<()>, Vec<String>) {
        let bus = Arc::new(FakeBus::default());
        *bus.to_consume.lock().unwrap() = events;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let result = run_subscriber(
            bus,
            storage,
            concurrency,
            Duration::from_secs(30),
            move |e| {
                let seen = seen_in.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    seen.lock().unwrap().push(e.key);
                    Ok(())
                }
            },
        )
        .await;
        let seen = seen.lock().unwrap().clone();
        (result, seen)
    }

    fn ingested(key: &str) -> Event {
        Event::inert(
            "papers.ingested",
            format!(r#"{{"key":"{key}"}}"#).into_bytes(),
        )
    }

    /// A server that answers every request with `status` and an empty body.
    async fn fixed_status_server(status: u16) -> String {
        fixed_response_server(status, "").await
    }

    /// [`fixed_status_server`] with `extra` response headers (CRLF-terminated).
    async fn fixed_response_server(status: u16, extra: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            format!("HTTP/1.1 {status} X\r\n{extra}content-length: 0\r\nconnection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .await;
                });
            }
        });
        url
    }

    const SCRIBE_REALM: &str = "www-authenticate: Bearer realm=\"hs-scribe\"\r\n";

    #[tokio::test]
    async fn only_the_scribe_servers_own_401_is_a_rejected_token() {
        async fn convert_err(url: &str) -> anyhow::Error {
            let client = ScribeClient::new_with_timeout(url, Duration::from_secs(5)).unwrap();
            client
                .convert_with_progress(pdf_with_pages(1), None, Some("s"), |_| {})
                .await
                .unwrap_err()
        }
        // The server's 401 (with its realm): typed, names the variable.
        let url = fixed_response_server(401, SCRIBE_REALM).await;
        let err = convert_err(&url).await;
        assert!(crate::client::is_backend_unauthorized(&err), "{err:#}");
        assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"), "{err:#}");
        // A bare 401 (a proxy / WAF), a 401 for another realm, a 403 and a
        // 500 are ordinary failures: retried, never fatal.
        for url in [
            fixed_status_server(401).await,
            fixed_response_server(401, "www-authenticate: Basic realm=\"corp-proxy\"\r\n").await,
            fixed_status_server(403).await,
            fixed_status_server(500).await,
        ] {
            let err = convert_err(&url).await;
            assert!(
                !crate::client::is_backend_unauthorized(&err),
                "{url}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn the_start_up_preflight_stops_on_a_rejected_token_and_waits_out_a_dead_server() {
        let t = Duration::from_secs(5);
        let rejected = fixed_response_server(401, SCRIBE_REALM).await;
        let ok = fixed_status_server(200).await;
        let err = preflight_token(&[ok.clone(), rejected], t)
            .await
            .unwrap_err();
        assert!(crate::client::is_backend_unauthorized(&err), "{err:#}");
        preflight_token(&[ok, "http://127.0.0.1:1".to_string()], t)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_preflight_catches_a_wrong_token_and_nothing_else() {
        let client =
            |url: &str| ScribeClient::new_with_timeout(url, Duration::from_secs(5)).unwrap();
        let rejected = fixed_response_server(401, SCRIBE_REALM).await;
        let err = client(&rejected).preflight().await.unwrap_err();
        assert!(crate::client::is_backend_unauthorized(&err), "{err:#}");
        assert!(client(&fixed_status_server(200).await)
            .preflight()
            .await
            .is_ok());
        // Not the credential's business: a proxy's 401, a 404 route, a dead host.
        assert!(client(&fixed_status_server(401).await)
            .preflight()
            .await
            .is_ok());
        assert!(client(&fixed_status_server(404).await)
            .preflight()
            .await
            .is_ok());
        let dead = client("http://127.0.0.1:1").preflight().await.unwrap_err();
        assert!(!crate::client::is_backend_unauthorized(&dead));
    }

    /// The real server's rejection is exactly what the client keys on.
    #[cfg(all(feature = "server", unix))]
    #[tokio::test]
    async fn the_real_servers_401_carries_the_realm_the_client_keys_on() {
        let resp = {
            let config = crate::config::AppConfig {
                converter: crate::config::ConverterMode::Olmocr,
                ..crate::config::AppConfig::default()
            };
            let state = Arc::new(crate::server::ServerState::new(config).unwrap());
            let token =
                hs_common::auth::backend::BackendToken::new("realm-test-token-0123456789abcdef-xx")
                    .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(
                async move { axum::serve(listener, crate::server::app(state, token)).await },
            );
            let anonymous = ScribeClient::new_with_client(
                &format!("http://{addr}"),
                hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                    hs_common::http::client_builder().build().unwrap(),
                    None,
                ),
                Duration::from_secs(5),
            );
            anonymous.preflight().await
        };
        let err = resp.unwrap_err();
        assert!(crate::client::is_backend_unauthorized(&err), "{err:#}");
    }

    #[tokio::test]
    async fn the_watcher_stops_with_an_error_when_the_backend_rejects_its_token() {
        let url = fixed_response_server(401, SCRIBE_REALM).await;
        let (_d, st) = storage();
        st.put("papers/ab/one.pdf", pdf_with_pages(1))
            .await
            .unwrap();
        let bus = Arc::new(FakeBus::default());
        *bus.to_consume.lock().unwrap() = vec![ingested("papers/ab/one.pdf")];
        let storage: Arc<dyn Storage> = Arc::new(st);
        let handled = Arc::new(AtomicUsize::new(0));
        let handled_in = handled.clone();
        let started = std::time::Instant::now();
        let result = run_subscriber(bus, storage, 2, Duration::from_secs(30), move |_event| {
            let url = url.clone();
            let handled = handled_in.clone();
            async move {
                handled.fetch_add(1, Ordering::SeqCst);
                let client = ScribeClient::new_with_timeout(&url, Duration::from_secs(5)).unwrap();
                client
                    .convert_with_progress(pdf_with_pages(1), None, Some("one"), |_| {})
                    .await
                    .map(|_| ())
                    .map_err(HandlerError::Transient)
            }
        })
        .await;
        let err = result.expect_err("a rejected token must end the watcher");
        assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"), "{err:#}");
        assert_eq!(handled.load(Ordering::SeqCst), 1);
        // It stopped at once; it did not wait for the stream to end or retry.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn events_pulled_but_never_started_are_handed_back_on_a_fatal_exit() {
        use hs_common::event_bus::Settlement;
        let mut logs = Vec::new();
        let mut events = Vec::new();
        for i in 0..4 {
            let (event, log) = Event::recording(
                "papers.ingested",
                format!(r#"{{"key":"papers/ab/doc{i}.pdf"}}"#).into_bytes(),
            );
            events.push(event);
            logs.push(log);
        }
        let bus = Arc::new(FakeBus::default());
        *bus.to_consume.lock().unwrap() = events;
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let handled = Arc::new(AtomicUsize::new(0));
        let handled_in = handled.clone();
        let result = run_subscriber(bus, storage, 1, Duration::from_secs(5), move |_e| {
            let handled = handled_in.clone();
            async move {
                handled.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(HandlerError::Transient(anyhow::Error::new(
                    crate::client::BackendUnauthorized {
                        server: "http://s".into(),
                        status: 401,
                    },
                )))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            handled.load(Ordering::SeqCst),
            1,
            "only the first event started"
        );
        // The one that ran is given back with the usual backoff; the three
        // that never started are given back for immediate redelivery.
        assert_eq!(logs[0].decisions(), [Settlement::Nak(Some(NAK_BACKOFF))]);
        for log in &logs[1..] {
            assert_eq!(log.decisions(), [Settlement::Nak(None)]);
        }
    }

    #[tokio::test]
    async fn a_stream_that_ends_is_an_error_not_a_clean_exit() {
        let (result, seen) = subscribe(Vec::new(), 2).await;
        let err = result.expect_err("an ended stream must fail");
        assert!(err.to_string().contains("event stream ended"), "{err}");
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn events_already_received_finish_before_the_ended_stream_is_reported() {
        let events = vec![
            ingested("a.pdf"),
            Event::inert("papers.ingested", b"not json".to_vec()),
            ingested("b.pdf"),
            ingested("c.pdf"),
        ];
        let (result, mut seen) = subscribe(events, 2).await;
        assert!(result.is_err());
        seen.sort();
        assert_eq!(
            seen,
            ["a.pdf", "b.pdf", "c.pdf"],
            "no handler may be cut off"
        );
    }

    #[tokio::test]
    async fn a_delivery_error_is_reported_with_its_cause_after_running_handlers_finish() {
        let bus = Arc::new(FakeBus::default());
        *bus.to_consume.lock().unwrap() = vec![ingested("a.pdf"), ingested("b.pdf")];
        *bus.then_fail.lock().unwrap() = Some("consumer deleted".into());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let err = run_subscriber(bus, storage, 2, Duration::from_secs(30), move |e| {
            let seen = seen_in.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                seen.lock().unwrap().push(e.key);
                Ok(())
            }
        })
        .await
        .expect_err("a delivery error must fail the subscriber");
        let shown = format!("{err:#}");
        assert!(shown.contains("consumer deleted"), "{shown}");
        assert!(!shown.contains("event stream ended"), "{shown}");
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["a.pdf", "b.pdf"], "no handler may be cut off");
    }

    #[tokio::test]
    async fn a_panicking_handler_terminates_its_event_and_the_subscriber_keeps_running() {
        let bus = Arc::new(FakeBus::default());
        let (poison, poison_log) =
            Event::recording("papers.ingested", br#"{"key":"poison.pdf"}"#.to_vec());
        let (good_a, a_log) = Event::recording("papers.ingested", br#"{"key":"a.pdf"}"#.to_vec());
        let (good_b, b_log) = Event::recording("papers.ingested", br#"{"key":"b.pdf"}"#.to_vec());
        *bus.to_consume.lock().unwrap() = vec![good_a, poison, good_b];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        // Concurrency 1: the permit the panicking task held must be released
        // or the event after it would never be dispatched.
        let result = run_subscriber(bus, storage, 1, Duration::from_secs(30), move |e| {
            let seen = seen_in.clone();
            async move {
                if e.key == "poison.pdf" {
                    panic!("lopdf exploded on {}", e.key);
                }
                seen.lock().unwrap().push(e.key);
                Ok(())
            }
        })
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("event stream ended"));
        use hs_common::event_bus::Settlement;
        assert_eq!(poison_log.decisions(), [Settlement::Term]);
        assert_eq!(a_log.decisions(), [Settlement::Ack]);
        assert_eq!(b_log.decisions(), [Settlement::Ack]);
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["a.pdf", "b.pdf"]);
    }

    #[tokio::test]
    async fn a_handler_that_never_finishes_is_abandoned_after_the_drain_timeout_and_named() {
        let bus = Arc::new(FakeBus::default());
        let (stuck, stuck_log) =
            Event::recording("papers.ingested", br#"{"key":"stuck.pdf"}"#.to_vec());
        let (quick, quick_log) =
            Event::recording("papers.ingested", br#"{"key":"quick.pdf"}"#.to_vec());
        *bus.to_consume.lock().unwrap() = vec![stuck, quick];
        *bus.then_fail.lock().unwrap() = Some("missed idle heartbeat".into());
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let started = std::time::Instant::now();
        let err = run_subscriber(
            bus,
            storage,
            2,
            Duration::from_millis(300),
            move |e| async move {
                if e.key == "stuck.pdf" {
                    std::future::pending::<()>().await;
                }
                Ok(())
            },
        )
        .await
        .expect_err("a delivery error must fail the subscriber");
        // Bounded: it did not wait for the stuck handler forever.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let shown = format!("{err:#}");
        assert!(shown.contains("missed idle heartbeat"), "{shown}");
        assert!(shown.contains("abandoned"), "{shown}");
        // The finished one was acked; the abandoned one was left un-acked
        // for the broker to redeliver after ack_wait.
        use hs_common::event_bus::Settlement;
        assert_eq!(quick_log.decisions(), [Settlement::Ack]);
        assert!(stuck_log.decisions().is_empty());
    }

    // ── against the real scribe server (olmocr mode, stand-in CLI) ─────

    #[cfg(all(feature = "server", unix))]
    mod served {
        use super::*;
        use crate::config::{AppConfig, ConverterMode};
        use crate::server::{app, ServerState};
        use hs_common::auth::backend::BackendToken;
        use std::os::unix::fs::PermissionsExt;

        const TOKEN: &str = "event-watch-served-token-0123456789abcdef";

        fn fake_olmocr(dir: &std::path::Path, tally: &str) -> std::path::PathBuf {
            let script = dir.join("olmocr.sh");
            let md = "A page of real converted text, long enough to be indexable. ".repeat(4);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nmkdir -p \"$1/markdown\" && printf '# Title\\n\\n{md}' > \"$1/markdown/o.md\"\n{tally}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        async fn serve(tally: &str) -> (tempfile::TempDir, ScribeClient) {
            let dir = tempfile::tempdir().unwrap();
            let config = AppConfig {
                converter: ConverterMode::Olmocr,
                olmocr_bin: fake_olmocr(dir.path(), tally)
                    .to_string_lossy()
                    .into_owned(),
                vlm_concurrency: 2,
                ..AppConfig::default()
            };
            let state = Arc::new(ServerState::new(config).unwrap());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let token = BackendToken::new(TOKEN).unwrap();
            tokio::spawn(async move { axum::serve(listener, app(state, token)).await });
            // The client authenticates exactly as production clients do.
            let http = hs_common::auth::client::AuthedHttp::plain_with_backend_token(
                hs_common::http::client_builder().build().unwrap(),
                Some(BackendToken::new(TOKEN).unwrap()),
            );
            let client = ScribeClient::new_with_client(
                &format!("http://{addr}"),
                http,
                Duration::from_secs(30),
            );
            (dir, client)
        }

        async fn fetched(st: &LocalFsStorage, key: &str, pages: usize) -> SourceObject {
            st.put(key, pdf_with_pages(pages)).await.unwrap();
            match prepare_source(st, &event(key)).await {
                Ok(SourcePrep::Fetched(s)) => s,
                _ => panic!("fetch"),
            }
        }

        #[tokio::test]
        async fn a_conversion_is_stored_stamped_and_announced_and_a_lost_announcement_is_retried() {
            skip_without_pdfium!("a_conversion_is_stored_stamped_and_announced");
            let (_srv, client) =
                serve("echo 'Completed pages: 3' >&2; echo 'Failed pages: 0' >&2").await;
            let (_d, st) = storage();
            let src = fetched(&st, "papers/ab/book.pdf", 3).await;
            let ev = event("papers/ab/book.pdf");
            let bus = FakeBus::default();
            bus.fail_next_publishes.store(1, Ordering::SeqCst);
            let (policy, limits) = (TimeoutPolicy::default(), EpubLimits::default());

            // First attempt: markdown stored and stamped, but the broker
            // refused the announcement -> Transient (the event is NAKed).
            let err = convert_and_upload(
                &st,
                &client,
                &bus,
                &ev,
                &policy,
                &limits,
                &src,
                Some("olmocr".into()),
                Vec::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(code_of(&err).0, "transient", "{err}");
            let md_key = hs_common::markdown::markdown_storage_key("book");
            assert!(st.exists(&md_key).await.unwrap());
            let row = hs_common::catalog::read_catalog_entry_via(&st, "catalog", "book")
                .await
                .unwrap()
                .unwrap();
            let conv = row.conversion.expect("conversion stamped");
            assert_eq!(
                (conv.server.as_str(), conv.converted_by.as_deref()),
                ("scribe-vlm", Some("olmocr"))
            );
            assert!(bus.published.lock().unwrap().is_empty());

            // Redelivery lands on AlreadyConverted, whose caller announces.
            let Ok(SourcePrep::AlreadyConverted(key)) = prepare_source(&st, &ev).await else {
                panic!("redelivery must find the stored markdown");
            };
            announce_completed(&bus, &key, &ev.key).await.unwrap();
            assert_eq!(bus.published.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn an_olmocr_run_with_failed_pages_escalates_and_stores_nothing() {
            skip_without_pdfium!("an_olmocr_run_with_failed_pages_escalates");
            let (_srv, client) =
                serve("echo 'Completed pages: 2' >&2; echo 'Failed pages: 1' >&2").await;
            let (_d, st) = storage();
            let src = fetched(&st, "papers/ab/gappy.pdf", 3).await;
            let bus = FakeBus::default();
            let err = convert_and_upload(
                &st,
                &client,
                &bus,
                &event("papers/ab/gappy.pdf"),
                &TimeoutPolicy::default(),
                &EpubLimits::default(),
                &src,
                None,
                Vec::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(
                code_of(&err),
                ("permanent", Some(FailureCode::OlmocrIncompletePages))
            );
            assert_eq!(
                match &err {
                    HandlerError::Permanent(e) => classify(e),
                    _ => unreachable!(),
                },
                FailureClass::Escalate("olmocr_incomplete_pages")
            );
            assert!(!st
                .exists(&hs_common::markdown::markdown_storage_key("gappy"))
                .await
                .unwrap());
            assert!(bus.published.lock().unwrap().is_empty());
        }
    }
}
