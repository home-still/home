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

/// Given an ingested paper key, fetch bytes from `storage`, dispatch to the
/// converter for its file type (PDF → scribe VLM, HTML → parser, EPUB →
/// parser), and put the markdown back under `markdown/{shard}/{stem}.md`.
/// Publishes `scribe.completed` with the markdown key on success. Skips
/// (returns Ok) if the markdown already exists — idempotent on retry.
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
pub async fn convert_and_upload(
    storage: &dyn Storage,
    scribe: &ScribeClient,
    bus: &dyn EventBus,
    event: &IngestedEvent,
    timeout_policy: &TimeoutPolicy,
) -> Result<String, HandlerError> {
    let filename = event
        .key
        .rsplit_once('/')
        .map(|(_, f)| f)
        .unwrap_or(&event.key);
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s, e.to_ascii_lowercase()),
        None => {
            return Err(HandlerError::Permanent(anyhow::anyhow!(
                "key {} has no extension",
                event.key
            )));
        }
    };
    let md_key = hs_common::markdown::markdown_storage_key(stem);

    let exists = storage
        .exists(&md_key)
        .await
        .map_err(|e| HandlerError::Transient(e.context(format!("head({md_key}) failed"))))?;
    if exists {
        tracing::info!(md_key = %md_key, "markdown already present; skipping");
        return Ok(md_key);
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
                    stem,
                    "source_missing",
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

    let start = std::time::Instant::now();
    let (markdown, server) = match ext.as_str() {
        "pdf" => {
            // Size the per-request timeout by PDF page count. Parsing
            // lopdf is CPU-bound; run it on the blocking pool so a 500-
            // page book doesn't stall the subscriber event loop.
            let bytes_for_meta = raw_bytes.clone();
            let pages =
                tokio::task::spawn_blocking(move || crate::pdf_meta::count_pages(&bytes_for_meta))
                    .await
                    .ok()
                    .flatten();
            let timeout = compute_convert_timeout(pages, timeout_policy);
            tracing::info!(
                key = %event.key,
                pages = pages.map(|n| n as i64).unwrap_or(-1),
                timeout_secs = timeout.as_secs(),
                "dispatching pdf to scribe with page-scaled timeout"
            );
            let md = scribe
                .convert(raw_bytes, Some(timeout))
                .await
                .map_err(|e| {
                    // Server-side format errors ("Invalid image size",
                    // "FormatError", PdfiumLibrary) are permanent — the
                    // PDF itself is broken. HTTP 415 with the
                    // unsupported_content_type body is permanent too
                    // (the /scribe gate rejected non-PDF bytes at the
                    // door). Everything else is a cluster-state problem
                    // and should NAK.
                    let msg = format!("{e:#}");
                    let perm = msg.contains("FormatError")
                        || msg.contains("Invalid image size")
                        || msg.contains("PdfiumLibrary")
                        || msg.contains("unsupported_content_type");
                    let ctx = e.context(format!("scribe convert failed for {}", event.key));
                    if perm {
                        HandlerError::Permanent(ctx)
                    } else {
                        HandlerError::Transient(ctx)
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
            let original_md = md.clone();
            let (md_clean, truncations) = crate::postprocess::clean_repetitions(&md);
            let longest_run = crate::postprocess::longest_repeated_run_bytes(&original_md);
            let pages_for_qc = hs_common::catalog::compute_page_offsets(&md_clean).len() as u64;
            match crate::postprocess::qc_verdict(truncations, pages_for_qc, longest_run) {
                crate::postprocess::QcVerdict::RejectLoop => {
                    if let Err(e) = hs_common::catalog::update_conversion_failed_via(
                        storage,
                        "catalog",
                        stem,
                        "vlm_repetition_loop",
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
            let html = String::from_utf8(raw_bytes).map_err(|e| {
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
            let md = crate::epub::convert_epub_to_markdown(&raw_bytes).map_err(|e| {
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
