//! `hs scribe queue`: announce that one stored source needs converting.
//!
//! The command publishes the same `papers.ingested` event a download or an
//! inbox drop does, so the scribe watcher converts the document and the
//! distill watcher indexes it. It converts nothing itself.

use anyhow::{Context, Result};
use hs_common::event_bus::{specs::PAPERS_INGESTED, EventBus};
use hs_common::storage::{is_not_found, Storage};
use hs_common::stores::DocumentLayout;
use serde::Serialize;

/// The `source` field of the event: who announced the document.
const EVENT_SOURCE: &str = "hs scribe queue";

/// What was queued.
#[derive(Debug, Serialize)]
pub(crate) struct QueueReport {
    pub stem: String,
    pub source_key: String,
    pub queued: bool,
    pub next: &'static str,
}

/// What to do once the event is published.
const NEXT: &str = "run `hs status` until the document is converted; the distill watcher \
                    embeds it once it is";

/// The key of the source of `stem`, probing each extension the store allows,
/// in order. A storage error other than "not found" stops the probe.
async fn find_source_key(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    stem: &str,
) -> Result<String> {
    let mut tried = Vec::with_capacity(layout.source_extensions.len());
    for extension in layout.source_extensions {
        let key = layout.source_key(stem, extension);
        match storage.head(&key).await {
            Ok(Some(_)) => return Ok(key),
            Ok(None) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e).with_context(|| format!("looking for {key}")),
        }
        tried.push(key);
    }
    anyhow::bail!(
        "No PDF, HTML, or EPUB found for '{stem}' (tried {})",
        tried.join(", ")
    )
}

/// Publish `papers.ingested` for the stored source of `stem`. A stem with no
/// source is an error and publishes nothing, so nothing is queued that the
/// watcher would then fail on.
pub(crate) async fn run(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    events: &dyn EventBus,
    stem: &str,
) -> Result<QueueReport> {
    hs_common::validate_stem(stem).with_context(|| format!("invalid stem {stem:?}"))?;
    let source_key = find_source_key(storage, layout, stem).await?;

    // `IngestedEvent` declares every field but `key` as optional: only the
    // key is known here, and the rest is left out rather than guessed.
    let payload = serde_json::to_vec(&serde_json::json!({
        "key": source_key,
        "source": EVENT_SOURCE,
    }))
    .with_context(|| format!("serializing papers.ingested for {source_key}"))?;
    events
        .publish(PAPERS_INGESTED.subject, &payload)
        .await
        .with_context(|| {
            format!(
                "queueing {source_key} failed (nothing was queued; the source is stored and \
                 the conversion did not start)"
            )
        })?;

    Ok(QueueReport {
        stem: stem.to_string(),
        source_key,
        queued: true,
        next: NEXT,
    })
}

/// `hs scribe queue`.
pub(crate) async fn cmd(stem: &str) -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (_, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;
    let events = ctx.events().await?;

    let report = run(storage.as_ref(), layout, events.as_ref(), stem).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::{stores, RecordingBus};
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;

    /// Queueing a stored HTML source announces its key on `papers.ingested`
    /// and converts nothing: no markdown appears.
    #[tokio::test]
    async fn queueing_publishes_the_source_key_and_converts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let key = layout.source_key("queued", "html");
        storage.put(&key, b"<html></html>".to_vec()).await.unwrap();
        let bus = RecordingBus::default();

        let report = run(&storage, layout, &bus, "queued").await.unwrap();

        assert!(report.queued);
        assert_eq!(report.source_key, key);
        assert!(!storage
            .exists(&layout.markdown_key("queued"))
            .await
            .unwrap());
        let published = bus.published.lock();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, PAPERS_INGESTED.subject);
        assert_eq!(published[0].1["key"], key.as_str());
        assert_eq!(published[0].1["source"], EVENT_SOURCE);
    }

    /// A stem with no source is an error that names every key tried, and
    /// nothing is published.
    #[tokio::test]
    async fn a_stem_with_no_source_is_refused_and_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let bus = RecordingBus::default();

        let err = run(&storage, layout, &bus, "ghost").await.unwrap_err();

        let message = format!("{err:#}");
        for extension in ["pdf", "html", "epub"] {
            assert!(message.contains(&format!("ghost.{extension}")), "{message}");
        }
        assert!(bus.published.lock().is_empty());
    }

    /// A broker that refuses the publish makes the command fail, and says
    /// that nothing was queued.
    #[tokio::test]
    async fn a_refused_publish_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        storage
            .put(&layout.source_key("doc", "pdf"), b"%PDF-1.4".to_vec())
            .await
            .unwrap();
        let bus = RecordingBus {
            refuse: true,
            ..Default::default()
        };

        let err = run(&storage, layout, &bus, "doc").await.unwrap_err();

        assert!(format!("{err:#}").contains("nothing was queued"), "{err:#}");
    }

    /// A stem that is a path is refused before storage is touched.
    #[tokio::test]
    async fn a_stem_that_is_a_path_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let bus = RecordingBus::default();

        assert!(run(&storage, layout, &bus, "../escape").await.is_err());
        assert!(bus.published.lock().is_empty());
    }
}
