use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use hs_common::event_bus::EventBus;
use hs_common::storage::Storage;
use serde::Deserialize;

use crate::client::ScribeClient;

/// Payload published by `paper` and any other ingestion source on
/// `papers.ingested`. Fields beyond `key` are optional — a minimal publisher
/// may only know the object key.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestedEvent {
    pub key: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Given an ingested paper key (PDF or HTML), fetch bytes from `storage`,
/// convert to markdown, and put the result back under the shared
/// `markdown/{shard}/{stem}.md` convention. PDFs go through the scribe VLM
/// server; HTML papers are converted locally. Publishes `scribe.completed`
/// with the markdown key on success. Skips (returns Ok) if the markdown
/// already exists — idempotent on retry.
pub async fn convert_and_upload(
    storage: &dyn Storage,
    scribe: &ScribeClient,
    bus: &dyn EventBus,
    event: &IngestedEvent,
) -> Result<String> {
    let filename = event
        .key
        .rsplit_once('/')
        .map(|(_, f)| f)
        .unwrap_or(&event.key);
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let md_key = hs_common::markdown::markdown_storage_key(stem);

    if storage
        .exists(&md_key)
        .await
        .with_context(|| format!("head({md_key}) failed"))?
    {
        tracing::info!(md_key = %md_key, "markdown already present; skipping");
        return Ok(md_key);
    }

    let raw_bytes = storage
        .get(&event.key)
        .await
        .with_context(|| format!("get({}) failed", event.key))?;

    let is_html = event.key.ends_with(".html") || event.key.ends_with(".htm");
    let markdown = if is_html {
        let html = String::from_utf8(raw_bytes)
            .with_context(|| format!("HTML at {} is not valid UTF-8", event.key))?;
        crate::html::convert_html_to_markdown(&html)
    } else {
        scribe
            .convert(raw_bytes)
            .await
            .with_context(|| format!("scribe convert failed for {}", event.key))?
    };

    // Empty/garbage output — record the failure on the catalog so the
    // backfill tool can see it, and skip both the storage write and the
    // `scribe.completed` publish (a downstream indexer would just skip it
    // again, leaving an unembedded markdown forever).
    if markdown.trim().is_empty() {
        let stem_only = stem.to_string();
        if let Err(e) = hs_common::catalog::update_conversion_failed_via(
            storage,
            "catalog",
            &stem_only,
            "event-watch",
            0.0,
            0,
            "empty_output",
        )
        .await
        {
            tracing::warn!(stem = %stem_only, error = %e, "failed-conversion catalog stamp failed");
        }
        tracing::warn!(
            stem = %stem_only,
            source_key = %event.key,
            "scribe convert produced empty markdown; not publishing scribe.completed",
        );
        return Ok(md_key);
    }

    storage
        .put(&md_key, markdown.into_bytes())
        .await
        .with_context(|| format!("put({md_key}) failed"))?;

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

/// Subscribe to `papers.ingested` and dispatch each event to `handler`.
///
/// The handler decides what to do — today a stub that logs, soon a
/// full convert-and-upload pipeline. Returns when the stream ends or the
/// caller drops the future.
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    handler: F,
) -> Result<()>
where
    F: Fn(IngestedEvent) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    let mut stream = bus.subscribe("papers.ingested").await?;
    tracing::info!("scribe subscribed to papers.ingested");

    while let Some(event) = stream.next().await {
        let parsed: IngestedEvent = match serde_json::from_slice(&event.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed papers.ingested event");
                continue;
            }
        };
        tracing::info!(key = %parsed.key, "scribe received ingested event");
        if let Err(e) = handler(parsed).await {
            tracing::error!(error = %e, "scribe handler failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_common::event_bus::NoOpBus;
    use hs_common::storage::LocalFsStorage;

    #[tokio::test]
    async fn parses_payload() {
        let payload = br#"{"key":"ab/cdef.pdf","sha256":"deadbeef","size_bytes":42,"source":"paper-download"}"#;
        let e: IngestedEvent = serde_json::from_slice(payload).unwrap();
        assert_eq!(e.key, "ab/cdef.pdf");
        assert_eq!(e.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(e.size_bytes, Some(42));
    }

    #[tokio::test]
    async fn live_subscriber_receives_published_event() {
        let Some(url) = std::env::var("HS_NATS_URL").ok() else {
            eprintln!("skipping: set HS_NATS_URL to run");
            return;
        };
        use hs_common::event_bus::nats::{NatsBus, NatsConfig};
        let bus: Arc<dyn EventBus> = Arc::new(NatsBus::connect(NatsConfig { url }).await.unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(tmp.path()));

        let received = Arc::new(tokio::sync::Mutex::new(Vec::<IngestedEvent>::new()));
        let received_clone = received.clone();

        let sub_task = tokio::spawn(run_subscriber(bus.clone(), storage.clone(), move |event| {
            let received = received_clone.clone();
            async move {
                received.lock().await.push(event);
                Ok(())
            }
        }));

        // Give the subscription a moment to register with the server.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let payload =
            br#"{"key":"ab/live.pdf","sha256":"feedface","size_bytes":7,"source":"test"}"#;
        bus.publish("papers.ingested", payload).await.unwrap();

        // Wait up to 2s for the subscriber to record the event.
        for _ in 0..40 {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let got = received.lock().await;
        assert_eq!(got.len(), 1, "expected one event, got {}", got.len());
        assert_eq!(got[0].key, "ab/live.pdf");

        sub_task.abort();
    }

    #[tokio::test]
    async fn noop_bus_completes_without_events() {
        // Subscribing to NoOpBus yields a pending stream; race with a short
        // timeout to prove the subscriber is set up correctly without needing
        // a live NATS server.
        let bus: Arc<dyn EventBus> = Arc::new(NoOpBus);
        let tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(tmp.path()));

        let fut = run_subscriber(bus, storage, |_e| async { Ok(()) });
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), fut).await;
        // NoOpBus stream is pending; the timeout is expected.
        assert!(r.is_err());
    }
}
