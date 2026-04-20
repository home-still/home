use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use hs_common::event_bus::EventBus;
use hs_common::storage::Storage;
use serde::{Deserialize, Serialize};

use crate::client::DistillClient;

/// Cap on re-publishes of a single event before we give up. See
/// `hs-scribe/src/event_watch.rs` for the rationale.
const MAX_RETRIES: u32 = 3;

/// Payload published by scribe (or any other markdown producer) on
/// `scribe.completed`. `key` is the storage key of the markdown object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompletedEvent {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// How many times this event has been re-published after a handler
    /// failure. `None` / `0` on first delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u32>,
}

/// Retry a storage write up to 3 times with exponential backoff (100ms,
/// 300ms, 900ms). Stamp writes are the bookkeeping side of the
/// embedding pipeline — a single S3 blip must not make the catalog and
/// Qdrant diverge, because that divergence is the source of the phantom
/// "unembedded" backlog the reconciler exists to clean up.
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
) -> Result<()> {
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
            // Stamp the failure so it's visible to the reconciler. Without
            // this, a dropped embed is indistinguishable from "never tried"
            // and the document is invisible until someone runs a full
            // markdown-vs-qdrant diff.
            tracing::error!(stem = %stem, key = %event.key, error = %e, "distill index failed");
            let reason = format!("embed_failed: {e}");
            if let Err(stamp_err) = write_with_retry("embed_failed stamp", stem, || {
                hs_common::catalog::update_embedding_skip_via(storage, "catalog", stem, &reason)
            })
            .await
            {
                tracing::error!(stem = %stem, error = %stamp_err, "failed to stamp embed_failed");
            }
            return Err(e.context(format!("distill index failed for {}", event.key)));
        }
    };

    // Stamp the embed outcome. Retry on transient S3/storage errors —
    // a lost stamp here is the historical root cause of phantom
    // "unembedded" docs (doc is in Qdrant but catalog doesn't know).
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
        // Retries exhausted: the reconciler will catch this later.
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

/// Subscribe to `scribe.completed` and dispatch each event to `handler`.
///
/// Dispatches run concurrently up to `concurrency` at a time (via a
/// semaphore), and failed handler invocations are re-published with an
/// incremented `retry_count` up to [`MAX_RETRIES`]. Beyond that the event
/// is dropped with an ERROR log.
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    concurrency: usize,
    handler: F,
) -> Result<()>
where
    F: Fn(CompletedEvent) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    // Queue-subscribe so a future second distill host load-balances
    // `scribe.completed` instead of both embedding every doc. Only one
    // distill runs today but the queue group is cheap and future-proofs.
    let mut stream = bus
        .queue_subscribe("scribe.completed", "distill-workers")
        .await?;
    let concurrency = concurrency.max(1);
    tracing::info!(
        concurrency,
        "distill subscribed to scribe.completed (queue: distill-workers)"
    );

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let handler = Arc::new(handler);

    while let Some(event) = stream.next().await {
        let parsed: CompletedEvent = match serde_json::from_slice(&event.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed scribe.completed event");
                continue;
            }
        };

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let handler = Arc::clone(&handler);
        let bus = Arc::clone(&bus);
        tokio::spawn(async move {
            let _permit = permit;
            let key = parsed.key.clone();
            tracing::info!(key = %key, "distill received completed event");
            if let Err(e) = handler(parsed.clone()).await {
                let next = parsed.retry_count.unwrap_or(0) + 1;
                if next <= MAX_RETRIES {
                    tracing::warn!(
                        key = %key,
                        retry = next,
                        error = %e,
                        "distill handler failed — republishing for retry"
                    );
                    let mut retry = parsed;
                    retry.retry_count = Some(next);
                    let payload = match serde_json::to_vec(&retry) {
                        Ok(b) => b,
                        Err(se) => {
                            tracing::error!(key = %key, error = %se, "retry payload serialize failed");
                            return;
                        }
                    };
                    if let Err(pe) = bus.publish("scribe.completed", &payload).await {
                        tracing::error!(key = %key, error = %pe, "retry republish failed");
                    }
                } else {
                    tracing::error!(
                        key = %key,
                        attempts = next,
                        error = %e,
                        "distill handler failed after max retries — giving up"
                    );
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

    #[tokio::test]
    async fn live_distill_subscriber_receives_event() {
        let Some(url) = std::env::var("HS_NATS_URL").ok() else {
            eprintln!("skipping: set HS_NATS_URL to run");
            return;
        };
        use hs_common::event_bus::nats::{NatsBus, NatsConfig};
        use hs_common::storage::LocalFsStorage;

        let bus: Arc<dyn EventBus> = Arc::new(NatsBus::connect(NatsConfig { url }).await.unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(tmp.path()));

        let received = Arc::new(tokio::sync::Mutex::new(Vec::<CompletedEvent>::new()));
        let received_clone = received.clone();
        let sub_task = tokio::spawn(run_subscriber(bus.clone(), storage, 1, move |event| {
            let received = received_clone.clone();
            async move {
                received.lock().await.push(event);
                Ok(())
            }
        }));

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        bus.publish(
            "scribe.completed",
            br#"{"key":"ab/live.md","source_key":"ab/live.pdf"}"#,
        )
        .await
        .unwrap();

        for _ in 0..40 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let got = received.lock().await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "ab/live.md");
        sub_task.abort();
    }
}
