use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use hs_common::event_bus::{specs, EventBus};
use hs_common::storage::Storage;
use serde::{Deserialize, Serialize};

use crate::client::{compute_convert_timeout, ScribeClient};
use crate::config::TimeoutPolicy;

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
/// and — for PDFs — count pages for the page-scaled timeout. Split out
/// of [`convert_and_upload`] so the chain dispatcher fetches and parses
/// ONCE and every backend attempt reuses the same bytes; escalation used
/// to re-download and re-parse the whole book from storage per backend.
pub enum SourcePrep {
    /// Markdown already present under this key — nothing to convert.
    AlreadyConverted(String),
    Fetched(SourceObject),
}

/// The fetched source plus everything derivable from it that the chain
/// needs per attempt. Bytes are shared (`Arc`) so per-backend dispatch
/// clones a refcount, not the file.
pub struct SourceObject {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub stem: String,
    pub ext: String,
    /// PDF page count for the page-scaled timeout. `None` for non-PDF
    /// sources or when lopdf can't parse a count.
    pub pdf_pages: Option<u32>,
}

pub async fn prepare_source(
    storage: &dyn Storage,
    event: &IngestedEvent,
) -> Result<SourcePrep, HandlerError> {
    let filename = event
        .key
        .rsplit_once('/')
        .map(|(_, f)| f)
        .unwrap_or(&event.key);
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), e.to_ascii_lowercase()),
        None => {
            return Err(HandlerError::Permanent(anyhow::anyhow!(
                "key {} has no extension",
                event.key
            )));
        }
    };
    let md_key = hs_common::markdown::markdown_storage_key(&stem);

    let exists = storage
        .exists(&md_key)
        .await
        .map_err(|e| HandlerError::Transient(e.context(format!("head({md_key}) failed"))))?;
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
                    "source_missing",
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
                return Err(HandlerError::Permanent(
                    e.context(format!("source bytes missing for {}", event.key)),
                ));
            }
            return Err(HandlerError::Transient(
                e.context(format!("get({}) failed", event.key)),
            ));
        }
    };
    let bytes = std::sync::Arc::new(raw_bytes);

    let pdf_pages = if ext == "pdf" {
        // Parsing lopdf is CPU-bound; run it on the blocking pool so a
        // 500-page book doesn't stall the subscriber event loop. Arc
        // clone — no byte copy.
        let bytes_for_meta = std::sync::Arc::clone(&bytes);
        tokio::task::spawn_blocking(move || crate::pdf_meta::count_pages(&bytes_for_meta))
            .await
            .ok()
            .flatten()
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

/// Given a prepared source, dispatch to the converter for its file type
/// (PDF → scribe VLM, HTML → parser, EPUB → parser), and put the
/// markdown back under `markdown/{shard}/{stem}.md`. Publishes
/// `scribe.completed` with the markdown key on success.
///
/// Error classification:
///
/// - `Permanent` — content is unconvertable no matter how many times we
///   retry: unsupported extension, HTML not UTF-8, paywall/loading HTML,
///   EPUB parse failure, and scribe-side PDF parse errors
///   (`FormatError`, `Invalid image size`, `PdfiumLibrary`). The
///   `/scribe` endpoint also returns HTTP 415 with a
///   `unsupported_content_type:{html,binary}` body for bytes that fail
///   the `%PDF` magic-byte gate; those bubble up as Permanent too.
/// - `Transient` — cluster state that will recover: storage GET/PUT
///   failure, scribe 5xx / connection reset / dispatch timeout, no ready
///   scribe servers. The caller NAKs with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn convert_and_upload(
    storage: &dyn Storage,
    scribe: &ScribeClient,
    bus: &dyn EventBus,
    event: &IngestedEvent,
    timeout_policy: &TimeoutPolicy,
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
            let pages = source.pdf_pages;
            let timeout = compute_convert_timeout(pages, timeout_policy);
            tracing::info!(
                key = %event.key,
                pages = pages.map(|n| n as i64).unwrap_or(-1),
                timeout_secs = timeout.as_secs(),
                "dispatching pdf to scribe with page-scaled timeout"
            );
            // Use the streaming endpoint so we get per-page PP-DocLayout-V3
            // region class names alongside the markdown — required for QC's
            // bibliography multiplier. Pass a no-op progress callback; this
            // handler has no UI to drive. The olmocr backend returns an
            // empty class vec; QC treats it as "not bibliography" → strict
            // default ceiling everywhere.
            let conversion = scribe
                .convert_with_progress((*source.bytes).clone(), Some(timeout), Some(stem), |_| {})
                .await
                .map_err(|e| {
                    // Permanent/Escalate-class failures (broken PDF,
                    // VLM repetition loop, olmocr rejecting every page)
                    // must NOT NAK — retrying the same backend produces
                    // the same result, and poison-pill papers would
                    // redeliver every 30 s on JetStream and hog every
                    // in-flight slot. They surface as
                    // HandlerError::Permanent so the chain dispatcher
                    // can consult `classify::classify_failure` for the
                    // stop-vs-next-backend decision. Only
                    // `FailureClass::Transient` (cluster-state problems)
                    // NAKs. One table decides: `classify.rs`.
                    let msg = format!("{e:#}");
                    let ctx = e.context(format!("scribe convert failed for {}", event.key));
                    match crate::classify::classify_failure(&msg) {
                        crate::classify::FailureClass::Transient => HandlerError::Transient(ctx),
                        _ => HandlerError::Permanent(ctx),
                    }
                })?;
            let md = conversion.markdown;
            let per_page_region_classes = conversion.per_page_region_classes;
            let per_page_diags = conversion.per_page_diags;
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
            let qc_started = std::time::Instant::now();
            let original_md = md.clone();
            let (md_clean, per_page_truncations) =
                crate::postprocess::clean_repetitions_per_page(&md);
            let truncations: usize = per_page_truncations.iter().map(|t| t.total()).sum();
            let longest_run = crate::postprocess::longest_repeated_run_bytes(&original_md);
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
            );
            // Optional --diag JSONL: opt-in via HS_SCRIBE_DIAG_DIR env var.
            // Server-side per-page records (collected during conversion)
            // and the document summary land in one append-only file per
            // stem for grep/jq inspection. Disabled in steady state via
            // the Option::is_none() check inside DiagWriter.
            let diag_dir = std::env::var_os("HS_SCRIBE_DIAG_DIR").map(std::path::PathBuf::from);
            let mut diag = crate::diag::DiagWriter::open(diag_dir.as_ref(), stem);
            for record in &per_page_diags {
                diag.write_page(stem, record.clone());
            }
            diag.write_document(crate::diag::DocSummaryRecord {
                stem: stem.to_string(),
                total_pages,
                per_page_truncation_counts: per_page_truncations.clone(),
                longest_run_bytes: longest_run,
                qc_verdict: format!("{verdict:?}"),
                wall_clock_ms: qc_started.elapsed().as_millis() as u64,
            });
            match verdict {
                crate::postprocess::QcVerdict::RejectLoop => {
                    if let Err(e) = hs_common::catalog::update_conversion_failed_via(
                        storage,
                        "catalog",
                        stem,
                        "vlm_repetition_loop",
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
                    return Err(HandlerError::Permanent(anyhow::anyhow!(
                        "VLM repetition loop on {} (truncations={}, longest_run={}B)",
                        event.key,
                        truncations,
                        longest_run
                    )));
                }
                crate::postprocess::QcVerdict::Accept => {
                    if truncations > 0 {
                        tracing::info!(
                            stem = %stem,
                            truncations,
                            longest_run,
                            "cleaned VLM repetition site(s)",
                        );
                    }
                }
            }
            (md_clean, "scribe-vlm")
        }
        "html" | "htm" => {
            let html = String::from_utf8((*source.bytes).clone()).map_err(|e| {
                HandlerError::Permanent(anyhow::anyhow!(
                    "HTML at {} is not valid UTF-8: {e}",
                    event.key
                ))
            })?;
            // Reject paywall / loading-stub / landing-page HTML before we
            // spend time extracting markdown that would just be stamped
            // `embedding_skip: zero_chunks_or_empty` downstream. Mirrors
            // the check the downloader runs at ingress — putting it here
            // too catches HTMLs that entered via any other path
            // (scribe_inbox, bulk import, etc.).
            if hs_common::html::is_paywall_html(&html) {
                return Err(HandlerError::Permanent(anyhow::anyhow!(
                    "{} looks like a paywall/loading-stub HTML; refusing to convert",
                    event.key
                )));
            }
            (crate::html::convert_html_to_markdown(&html), "html-parser")
        }
        "epub" => {
            let md = crate::epub::convert_epub_to_markdown(&source.bytes).map_err(|e| {
                HandlerError::Permanent(anyhow::anyhow!("EPUB parse failed for {}: {e}", event.key))
            })?;
            (md, "epub-parser")
        }
        other => {
            return Err(HandlerError::Permanent(anyhow::anyhow!(
                "unsupported source type `.{other}` for {} — supported: .pdf, .html, .htm, .epub",
                event.key
            )));
        }
    };
    let duration_secs = start.elapsed().as_secs_f64();

    let page_offsets = hs_common::catalog::compute_page_offsets(&markdown);
    let total_pages = page_offsets.len() as u64;

    storage
        .put(&md_key, markdown.into_bytes())
        .await
        .map_err(|e| HandlerError::Transient(e.context(format!("put({md_key}) failed"))))?;

    // Stamp the catalog with the converter used so downstream can tell
    // which pipeline produced this markdown without guessing. Stamp
    // failures are logged but don't poison the event — the markdown is
    // already committed to storage; retry-driving on a stamp hiccup
    // would re-run the VLM for nothing.
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
        tracing::warn!(stem = %stem, error = %e, "conversion catalog stamp failed");
    }

    let payload = serde_json::json!({
        "key": md_key,
        "source_key": event.key,
    });
    if let Err(e) = bus
        .publish(
            "scribe.completed",
            serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
        )
        .await
    {
        tracing::warn!(error = %e, "scribe.completed publish failed");
    }

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
///
/// `max_deliver` on the consumer spec bounds total redeliveries, so a
/// stuck-in-transient-loop message eventually surfaces as a permanent
/// failure in operator logs.
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    concurrency: usize,
    handler: F,
) -> Result<()>
where
    F: Fn(IngestedEvent) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), HandlerError>> + Send + 'static,
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

    while let Some(event) = stream.next().await {
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

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed → shutting down
        };
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let _permit = permit; // drop at scope end releases the slot
            let key = parsed.key.clone();
            tracing::info!(key = %key, "scribe received ingested event");
            match handler(parsed).await {
                Ok(()) => {
                    if let Err(e) = event.ack().await {
                        tracing::warn!(key = %key, error = %e, "ack failed");
                    }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_payload() {
        let payload = br#"{"key":"ab/cdef.pdf","sha256":"deadbeef","size_bytes":42,"source":"paper-download"}"#;
        let e: IngestedEvent = serde_json::from_slice(payload).unwrap();
        assert_eq!(e.key, "ab/cdef.pdf");
        assert_eq!(e.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(e.size_bytes, Some(42));
    }

    #[test]
    fn extension_parsing_rejects_no_dot() {
        // Defense-in-depth: every key we ingest has `.pdf|.html|.htm|.epub`,
        // but if some path slips through, we want a clean Permanent error
        // (terminate the message) rather than a panic.
        let filename = "no_extension_here";
        assert!(filename.rsplit_once('.').is_none());
    }
}
