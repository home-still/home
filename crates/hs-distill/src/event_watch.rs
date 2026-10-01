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

/// Delays before the second, third and fourth attempt of a stamp write
/// (100 ms, 300 ms, 900 ms).
const STAMP_BACKOFFS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_millis(900),
];

/// Retry a storage write up to 3 times with exponential backoff. Stamp
/// writes are the bookkeeping side of the embedding pipeline — a single S3
/// blip must not make the catalog and Qdrant diverge.
async fn write_with_retry<F, Fut>(op_name: &str, stem: &str, op: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    retry_with(&STAMP_BACKOFFS, op_name, stem, op).await
}

async fn retry_with<F, Fut>(
    backoffs: &[Duration],
    op_name: &str,
    stem: &str,
    mut op: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    for (attempt, wait) in std::iter::once(Duration::ZERO)
        .chain(backoffs.iter().copied())
        .enumerate()
    {
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

/// Retrying cannot fix a key that names nothing legal, so those are
/// permanent; every other storage error may clear up.
fn classify(err: anyhow::Error) -> HandlerError {
    if hs_common::storage::is_invalid_key(&err) {
        HandlerError::Permanent(err)
    } else {
        HandlerError::Transient(err)
    }
}

/// The document stem an event's markdown key designates.
fn stem_of(key: &str) -> &str {
    key.rsplit('/')
        .next()
        .unwrap_or(key)
        .trim_end_matches(".md")
}

/// Pull the markdown at `event.key` from `storage` and ask the distill
/// server to index it. Loads the catalog entry for the stem so metadata
/// (title, authors, DOI, year) lands on Qdrant payloads. Publishes
/// `distill.completed` on success.
///
/// Every step that must happen for the event to be complete is an error
/// when it fails, so the event is redelivered rather than acknowledged with
/// work missing: indexing is idempotent (the server replaces the document's
/// chunks), and the redelivery repeats the stamp and the publish.
/// A key that is not a legal storage key or document stem is permanent.
pub async fn index_and_publish(
    storage: &dyn Storage,
    distill: &DistillClient,
    bus: &dyn EventBus,
    event: &CompletedEvent,
) -> Result<(), HandlerError> {
    let stem = stem_of(&event.key);
    hs_common::validate_stem(stem).map_err(|e| {
        HandlerError::Permanent(anyhow::anyhow!(
            "event key {:?} does not name a document: {e}",
            event.key
        ))
    })?;
    // Transient read errors propagate; a missing row is Ok(None) — the
    // indexer is happy to proceed without prior metadata. A corrupt row
    // bubbles up as Err here (correctness fix from P0-10).
    let catalog = hs_common::catalog::read_catalog_entry_via(storage, "catalog", stem)
        .await
        .map_err(|e| classify(e.context(format!("catalog read for {stem}"))))?;

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
            return Err(classify(
                e.context(format!("distill index failed for {}", event.key)),
            ));
        }
    };

    write_with_retry("embedding stamp", stem, || {
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
    .map_err(|e| {
        HandlerError::Transient(e.context(format!(
            "embedding catalog stamp for {stem} lost after retries"
        )))
    })?;

    let payload = serde_json::to_vec(&serde_json::json!({
        "key": event.key,
        "doc_id": result.doc_id,
        "chunks_indexed": result.chunks_indexed,
    }))
    .map_err(|e| {
        HandlerError::Permanent(
            anyhow::Error::new(e).context("serializing the distill.completed payload"),
        )
    })?;
    bus.publish("distill.completed", &payload)
        .await
        .map_err(|e| HandlerError::Transient(e.context("distill.completed publish failed")))?;
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

        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("distill worker semaphore closed"))?;
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

    // The stream only ends when the broker dropped the consumer or the
    // connection (a competing watcher deleting the durable, a broker
    // restart). Consumption has stopped, so this is a failure — returning
    // Ok would let the process exit 0 and stay down. Let handlers that are
    // already running finish their ack/nak first.
    let _ = sem.acquire_many(concurrency as u32).await;
    Err(anyhow::anyhow!(
        "event stream ended: the consumer or broker connection for {} is gone",
        specs::SCRIBE_COMPLETED.subject
    ))
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
        assert_eq!(
            stem_of("markdown/10/10.1609_aaai.v38i16.29728.md"),
            "10.1609_aaai.v38i16.29728"
        );
        assert_eq!(stem_of("plain.md"), "plain");
        assert_eq!(stem_of("no-extension"), "no-extension");
    }

    // ── Handler outcomes (RA-47, RA-87) ────────────────────────────────

    use crate::testutil::{serve, FakeHttp, Reply};
    use async_trait::async_trait;
    use hs_common::event_bus::{ConsumerSpec, Event, EventStream};
    use hs_common::storage::{LocalFsStorage, ObjectMeta};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Local storage whose writes can be made to fail.
    struct FlakyStorage {
        inner: LocalFsStorage,
        fail_put: AtomicBool,
    }

    #[async_trait]
    impl Storage for FlakyStorage {
        async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
            self.inner.get(key).await
        }
        async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
            if self.fail_put.load(Ordering::SeqCst) {
                anyhow::bail!("storage unavailable");
            }
            self.inner.put(key, bytes).await
        }
        async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
            self.inner.head(key).await
        }
        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
            self.inner.list(prefix).await
        }
        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key).await
        }
    }

    /// Bus that records publishes and replays a fixed list of events.
    #[derive(Default)]
    struct FakeBus {
        published: Mutex<Vec<(String, Vec<u8>)>>,
        fail_publish: AtomicBool,
        events: Mutex<Vec<Event>>,
    }

    #[async_trait]
    impl EventBus for FakeBus {
        async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
            if self.fail_publish.load(Ordering::SeqCst) {
                anyhow::bail!("broker down");
            }
            self.published
                .lock()
                .push((subject.into(), payload.to_vec()));
            Ok(())
        }
        async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
            let events = std::mem::take(&mut *self.events.lock());
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    struct Rig {
        storage: FlakyStorage,
        bus: FakeBus,
        server: FakeHttp,
        client: DistillClient,
        _dir: tempfile::TempDir,
    }

    const OK_INDEX: &str = r#"{"doc_id":"doc","chunks_indexed":4,"embedding_device":"Cuda"}"#;

    async fn rig(reply: fn() -> Reply) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let storage = FlakyStorage {
            inner: LocalFsStorage::new(dir.path().to_path_buf()),
            fail_put: AtomicBool::new(false),
        };
        storage
            .inner
            .put(
                "markdown/do/doc.md",
                b"# Title\n\nSome converted text.".to_vec(),
            )
            .await
            .unwrap();
        let server = serve(move |_| reply()).await;
        let client = DistillClient::new(&server.url()).unwrap();
        Rig {
            storage,
            bus: FakeBus::default(),
            server,
            client,
            _dir: dir,
        }
    }

    fn event(key: &str) -> CompletedEvent {
        CompletedEvent {
            key: key.into(),
            source_key: None,
        }
    }

    async fn run(rig: &Rig, key: &str) -> Result<(), HandlerError> {
        index_and_publish(&rig.storage, &rig.client, &rig.bus, &event(key)).await
    }

    fn kind_of(r: Result<(), HandlerError>) -> &'static str {
        match r {
            Ok(()) => "ok",
            Err(HandlerError::Permanent(_)) => "permanent",
            Err(HandlerError::Transient(_)) => "transient",
        }
    }

    #[tokio::test]
    async fn a_completed_index_is_stamped_and_announced() {
        let rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        run(&rig, "markdown/do/doc.md")
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        let row = hs_common::catalog::read_catalog_entry_via(&rig.storage, "catalog", "doc")
            .await
            .unwrap()
            .expect("stamped row");
        assert_eq!(row.embedding.unwrap().chunks_indexed, 4);

        let published = rig.bus.published.lock();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "distill.completed");
        let body: serde_json::Value = serde_json::from_slice(&published[0].1).unwrap();
        assert_eq!(body["key"], "markdown/do/doc.md");
        assert_eq!(body["chunks_indexed"], 4);
    }

    #[tokio::test]
    async fn a_lost_embedding_stamp_is_transient_so_the_event_is_redelivered() {
        // RA-87: the handler used to log the lost stamp and ACK.
        let rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        rig.storage.fail_put.store(true, Ordering::SeqCst);
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "transient");
        assert!(
            rig.bus.published.lock().is_empty(),
            "distill.completed must not announce an unstamped document"
        );
    }

    #[tokio::test]
    async fn a_failed_completed_publish_is_transient() {
        let rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        rig.bus.fail_publish.store(true, Ordering::SeqCst);
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "transient");
        // The stamp was written; the redelivery re-stamps idempotently.
        assert!(
            hs_common::catalog::read_catalog_entry_via(&rig.storage, "catalog", "doc")
                .await
                .unwrap()
                .unwrap()
                .embedding
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_server_error_is_transient_and_leaves_a_retryable_marker() {
        let rig = rig(|| Reply::Json(500, "qdrant down".into())).await;
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "transient");
        let row = hs_common::catalog::read_catalog_entry_via(&rig.storage, "catalog", "doc")
            .await
            .unwrap()
            .unwrap();
        assert!(row
            .embedding_skip
            .unwrap()
            .reason
            .starts_with("embed_failed"));
        assert!(rig.bus.published.lock().is_empty());
    }

    #[tokio::test]
    async fn keys_that_cannot_name_a_document_are_permanent() {
        let rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        for key in [
            "markdown/../../etc/passwd.md", // escapes the storage root
            "/etc/passwd.md",               // absolute
            "markdown\\do\\doc.md",         // backslash
            "markdown/do/",                 // no stem
            "markdown/do/..md",             // dot-segment stem
        ] {
            assert_eq!(kind_of(run(&rig, key).await), "permanent", "{key:?}");
        }
        assert!(
            rig.server.recorded().is_empty(),
            "nothing may reach the server"
        );
        assert!(rig.bus.published.lock().is_empty());
    }

    #[tokio::test]
    async fn a_missing_markdown_object_is_not_permanent() {
        // NotFound can be replication lag; only illegal keys are terminal.
        let rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        assert_eq!(
            kind_of(run(&rig, "markdown/zz/absent.md").await),
            "transient"
        );
    }

    #[tokio::test]
    async fn retry_stops_at_the_first_success_and_reports_the_last_error() {
        let calls = AtomicUsize::new(0);
        retry_with(&[Duration::ZERO; 3], "op", "s", || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                anyhow::bail!("blip");
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let calls = AtomicUsize::new(0);
        let err = retry_with(&[Duration::ZERO; 3], "op", "s", || async {
            anyhow::bail!("attempt {}", calls.fetch_add(1, Ordering::SeqCst))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 4, "initial try + 3 retries");
        assert_eq!(err.to_string(), "attempt 3");
    }

    // ── run_subscriber ─────────────────────────────────────────────────

    fn completed(key: &str) -> Event {
        Event::inert(
            "scribe.completed",
            format!(r#"{{"key":"{key}"}}"#).into_bytes(),
        )
    }

    async fn subscribe(
        events: Vec<Event>,
        concurrency: usize,
    ) -> (anyhow::Result<()>, Vec<String>) {
        let bus = Arc::new(FakeBus::default());
        *bus.events.lock() = events;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_handler = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let result = run_subscriber(bus, storage, concurrency, move |e| {
            let seen = seen_in_handler.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                seen.lock().push(e.key);
                Ok(())
            }
        })
        .await;
        let seen = seen.lock().clone();
        (result, seen)
    }

    #[tokio::test]
    async fn a_stream_that_ends_is_an_error_not_a_clean_exit() {
        // RA-47: Ok(()) here made the process exit 0 with consumption dead.
        let (result, seen) = subscribe(Vec::new(), 2).await;
        let err = result.expect_err("an ended stream must fail");
        assert!(err.to_string().contains("event stream ended"), "{err}");
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn events_already_received_finish_before_the_ended_stream_is_reported() {
        let events = vec![completed("a"), completed("b"), completed("c")];
        let (result, mut seen) = subscribe(events, 2).await;
        assert!(result.is_err());
        seen.sort();
        assert_eq!(seen, ["a", "b", "c"], "no handler may be cut off");
    }

    #[tokio::test]
    async fn a_malformed_event_does_not_stop_the_ones_after_it() {
        let events = vec![
            Event::inert("scribe.completed", b"not json".to_vec()),
            completed("good"),
        ];
        let (result, seen) = subscribe(events, 1).await;
        assert!(result.is_err());
        assert_eq!(seen, ["good"]);
    }
}
