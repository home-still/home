use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use hs_common::event_bus::EventBus;
use hs_common::storage::Storage;
use serde::Deserialize;

use crate::client::DistillClient;

/// Payload published by scribe (or any other markdown producer) on
/// `scribe.completed`. `key` is the storage key of the markdown object.
#[derive(Debug, Clone, Deserialize)]
pub struct CompletedEvent {
    pub key: String,
    #[serde(default)]
    pub source_key: Option<String>,
}

/// Pull the markdown at `event.key` from `storage` and ask the distill
/// server to index it. Publishes `distill.completed` on success.
pub async fn index_and_publish(
    storage: &dyn Storage,
    distill: &DistillClient,
    bus: &dyn EventBus,
    event: &CompletedEvent,
) -> Result<()> {
    let result = distill
        .index_from_storage(storage, &event.key)
        .await
        .with_context(|| format!("distill index failed for {}", event.key))?;

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
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    handler: F,
) -> Result<()>
where
    F: Fn(CompletedEvent) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    let mut stream = bus.subscribe("scribe.completed").await?;
    tracing::info!("distill subscribed to scribe.completed");

    while let Some(event) = stream.next().await {
        let parsed: CompletedEvent = match serde_json::from_slice(&event.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed scribe.completed event");
                continue;
            }
        };
        tracing::info!(key = %parsed.key, "distill received completed event");
        if let Err(e) = handler(parsed).await {
            tracing::error!(error = %e, "distill handler failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_payload() {
        let payload = br#"{"key":"ab/cdef.md","source_key":"ab/cdef.pdf"}"#;
        let e: CompletedEvent = serde_json::from_slice(payload).unwrap();
        assert_eq!(e.key, "ab/cdef.md");
        assert_eq!(e.source_key.as_deref(), Some("ab/cdef.pdf"));
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
        let sub_task = tokio::spawn(run_subscriber(bus.clone(), storage, move |event| {
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
