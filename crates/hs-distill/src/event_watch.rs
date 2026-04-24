use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use hs_common::event_bus::{specs, EventBus};
use hs_common::storage::Storage;
use serde::{Deserialize, Serialize};

use crate::client::DistillClient;

/// Handler outcome for the distill consumer. Same Permanent/Transient
/// split as scribe — see `crates/hs-scribe/src/event_watch.rs` for the
/// rationale.
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

/// Payload published by scribe (or any other markdown producer) on
/// `scribe.completed`. `key` is the storage key of the markdown object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompletedEvent {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
}

/// Retry a storage write up to 3 times with exponential backoff (100ms,
/// 300ms, 900ms). Stamp writes are the bookkeeping side of the
/// embedding pipeline — a single S3 blip must not make the catalog and
/// Qdrant diverge.
async fn write_with_retry<F, Fut>(op_name: &str, stem: &str, mut op: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let backoffs = [
        Duration::from_millis(100),
        Duration::from_millis(300),
        Duration::from_millis(900),
    ];
    let mut last_err: Option<anyhow::Error> = None;
    for (attempt, wait) in std::iter::once(Duration::ZERO).chain(backoffs).enumerate() {
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        match op().await {
            Ok(()) => {
                if attempt > 0 {
                    tracing::info!(stem = %stem, attempt, "{op_name} succeeded after retry");
                }
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(stem = %stem, attempt, error = %e, "{op_name} failed, will retry");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{op_name}: retries exhausted")))
}

/// Pull the markdown at `event.key` from `storage` and ask the distill
/// server to index it. Loads the catalog entry for the stem so metadata
/// (title, authors, DOI, year) lands on Qdrant payloads. Publishes
/// `distill.completed` on success.
pub async fn index_and_publish(
    storage: &dyn Storage,
    distill: &DistillClient,
    bus: &dyn EventBus,
    event: &CompletedEvent,
) -> Result<(), HandlerError> {
    let stem = event
        .key
        .rsplit('/')
        .next()
        .unwrap_or(&event.key)
        .trim_end_matches(".md");
    let catalog = hs_common::catalog::read_catalog_entry_via(storage, "catalog", stem).await;

    let result = match distill
        .index_from_storage_with_catalog(storage, &event.key, catalog.as_ref())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Stamp the failure so the reconciler can find it later.
            tracing::error!(stem = %stem, key = %event.key, error = %e, "distill index failed");
            let reason = format!("embed_failed: {e}");
            if let Err(stamp_err) = write_with_retry("embed_failed stamp", stem, || {
                hs_common::catalog::update_embedding_skip_via(storage, "catalog", stem, &reason)
            })
            .await
            {
                tracing::error!(stem = %stem, error = %stamp_err, "failed to stamp embed_failed");
            }
            // Index failures are transient by default — a flaky Qdrant
            // or a slow VRAM recovery shouldn't throw away the event.
            // JetStream's max_deliver bounds the retry count; a truly
            // broken markdown will eventually TERM on its own.
            return Err(HandlerError::Transient(
                e.context(format!("distill index failed for {}", event.key)),
            ));
        }
    };

    if let Err(e) = write_with_retry("embedding stamp", stem, || {
        hs_common::catalog::record_embedding_outcome_via(
            storage,
            "catalog",
            stem,
            "event-watch",
            result.chunks_indexed,
            &result.embedding_device,
        )
    })
    .await
    {
        tracing::error!(stem = %stem, error = %e, "embedding catalog stamp lost after retries");
    }

    let payload = serde_json::json!({
        "key": event.key,
        "doc_id": result.doc_id,
        "chunks_indexed": result.chunks_indexed,
    });
    if let Err(e) = bus
        .publish(
            "distill.completed",
            serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
        )
        .await
    {
        tracing::warn!(error = %e, "distill.completed publish failed");
    }
    Ok(())
}

/// Pull-consume `scribe.completed` and dispatch each event to
/// `handler`. See the parallel scribe `run_subscriber` for ack policy.
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    concurrency: usize,
    handler: F,
) -> Result<()>
where
    F: Fn(CompletedEvent) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), HandlerError>> + Send + 'static,
{
    let mut stream = bus.consume(&specs::SCRIBE_COMPLETED).await?;
    let concurrency = concurrency.max(1);
    tracing::info!(
        concurrency,
        "distill consuming scribe.completed (durable: {})",
        specs::SCRIBE_COMPLETED.durable_name
    );

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let handler = Arc::new(handler);

    while let Some(event) = stream.next().await {
        let parsed: CompletedEvent = match serde_json::from_slice(&event.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    payload_len = event.payload.len(),
                    "malformed scribe.completed payload — terminating (will not redeliver)"
                );
                if let Err(term_err) = event.term().await {
                    tracing::warn!(error = %term_err, "failed to term malformed event");
                }
                continue;
            }
        };

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let _permit = permit;
            let key = parsed.key.clone();
            tracing::info!(key = %key, "distill received completed event");
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
                            "distill handler permanent failure — terminating (will not redeliver)"
                        );
                        if let Err(e) = event.term().await {
                            tracing::warn!(key = %key, error = %e, "term failed");
                        }
                    } else {
                        tracing::warn!(
                            key = %key,
                            error = ?inner,
                            backoff_secs = NAK_BACKOFF.as_secs(),
                            "distill handler transient failure — redelivering after backoff"
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
        let payload = br#"{"key":"markdown/ab/cdef.md","source_key":"ab/cdef.pdf"}"#;
        let e: CompletedEvent = serde_json::from_slice(payload).unwrap();
        assert_eq!(e.key, "markdown/ab/cdef.md");
        assert_eq!(e.source_key.as_deref(), Some("ab/cdef.pdf"));
    }

    #[test]
    fn stem_from_markdown_key() {
        let key = "markdown/10/10.1609_aaai.v38i16.29728.md";
        let stem = key
            .rsplit('/')
            .next()
            .unwrap_or(key)
            .trim_end_matches(".md");
        assert_eq!(stem, "10.1609_aaai.v38i16.29728");
    }
}
