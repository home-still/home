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
    /// Misconfiguration every event will hit (the server rejects our
    /// `HS_BACKEND_TOKEN`): the event is NAKed untouched and
    /// [`run_subscriber`] stops consuming and returns an error.
    Fatal(anyhow::Error),
}

impl HandlerError {
    fn as_error(&self) -> &anyhow::Error {
        match self {
            HandlerError::Permanent(e) | HandlerError::Transient(e) | HandlerError::Fatal(e) => e,
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandlerError::Permanent(e) => write!(f, "permanent: {e:#}"),
            HandlerError::Transient(e) => write!(f, "transient: {e:#}"),
            HandlerError::Fatal(e) => write!(f, "fatal: {e:#}"),
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

/// Retrying cannot fix a key that names nothing legal, or a document the
/// server has refused for what it contains (400/413/422), so those are
/// permanent; every other error — storage or server outages included — may
/// clear up.
fn classify(err: anyhow::Error) -> HandlerError {
    if err.chain().any(|c| c.is::<crate::client::Unauthorized>()) {
        return HandlerError::Fatal(err);
    }
    let refused = err
        .chain()
        .filter_map(|c| c.downcast_ref::<crate::client::ServerError>())
        .any(|e| e.is_permanent_rejection());
    if hs_common::storage::is_invalid_key(&err) || refused {
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
            tracing::error!(stem = %stem, key = %event.key, error = %e, "distill index failed");
            let classified = classify(e.context(format!("distill index failed for {}", event.key)));
            // Rejected credentials are a configuration error, not a verdict
            // on this document: stamp nothing.
            if matches!(classified, HandlerError::Fatal(_)) {
                return Err(classified);
            }
            let e = match &classified {
                HandlerError::Permanent(e)
                | HandlerError::Transient(e)
                | HandlerError::Fatal(e) => {
                    format!("{e:#}")
                }
            };
            // Stamp the failure so the reconciler can find it later.
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
            return Err(classified);
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

/// NAK a received-but-not-started event, bounded and logged.
async fn nak_unstarted(event: &hs_common::event_bus::Event, delay: Option<Duration>) {
    match tokio::time::timeout(Duration::from_secs(5), event.nak(delay)).await {
        Ok(Ok(())) => tracing::info!("returned an unstarted event to the broker"),
        Ok(Err(e)) => tracing::warn!(error = %e, "nak of an unstarted event failed"),
        Err(_) => tracing::warn!("nak of an unstarted event timed out"),
    }
}

async fn orderly_stop(
    sem: &Arc<tokio::sync::Semaphore>,
    concurrency: usize,
    drain_timeout: Duration,
    in_flight: &hs_common::event_bus::InFlight,
) {
    tracing::info!("shutdown requested: draining running handlers");
    let abandoned =
        hs_common::event_bus::drain_in_flight(sem, concurrency, drain_timeout, in_flight).await;
    if !abandoned.is_empty() {
        tracing::error!(
            abandoned = abandoned.len(),
            "handlers still running after the drain timeout; their events are redelivered after ack_wait"
        );
    }
}

fn fatal_stop(cause: anyhow::Error) -> anyhow::Error {
    cause.context(
        "fatal configuration error: the distill server rejected HS_BACKEND_TOKEN \
         (set the same value in secrets.env as the server); the watcher stopped consuming",
    )
}

/// Start-up check, run before [`run_subscriber`] pulls any event: one cheap
/// authenticated, side-effect-free request (`/status`). A server that
/// rejects our `HS_BACKEND_TOKEN` stops the watcher here, with nothing
/// consumed. Any other failure (server down, slow) is only warned about: the
/// per-event handling already copes with an unavailable server.
pub async fn preflight(distill: &DistillClient) -> Result<()> {
    match distill.status().await {
        Ok(_) => Ok(()),
        Err(e) if e.chain().any(|c| c.is::<crate::client::Unauthorized>()) => Err(fatal_stop(e)),
        Err(e) => {
            tracing::warn!(error = %e, "distill preflight request failed; continuing");
            Ok(())
        }
    }
}

/// Resolves on SIGTERM or ctrl-c.
async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Pull-consume `scribe.completed` and dispatch each event to
/// `handler`. See the parallel scribe `run_subscriber` for ack policy.
/// Returns `Ok(())` only on an orderly stop (SIGTERM / ctrl-c).
pub async fn run_subscriber<F, Fut>(
    bus: Arc<dyn EventBus>,
    storage: Arc<dyn Storage>,
    concurrency: usize,
    drain_timeout: std::time::Duration,
    handler: F,
) -> Result<()>
where
    F: Fn(CompletedEvent) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), HandlerError>> + Send + 'static,
{
    run_subscriber_until(
        bus,
        storage,
        concurrency,
        drain_timeout,
        handler,
        termination_signal(),
    )
    .await
}

/// [`run_subscriber`] with an explicit shutdown trigger.
///
/// Events are pulled one at a time and the held one waits for a free
/// permit. On every exit path the held, not-yet-started event is NAKed
/// (immediately on an orderly stop, after [`NAK_BACKOFF`] on a fatal error)
/// so the broker redelivers it promptly instead of after `ack_wait`. Events
/// the broker has buffered for us but we never pulled cannot be NAKed here;
/// they were never delivered to this process.
pub async fn run_subscriber_until<F, Fut>(
    bus: Arc<dyn EventBus>,
    _storage: Arc<dyn Storage>,
    concurrency: usize,
    drain_timeout: std::time::Duration,
    handler: F,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()>
where
    F: Fn(CompletedEvent) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), HandlerError>> + Send + 'static,
{
    tokio::pin!(shutdown);
    let mut stream = bus.consume(&specs::SCRIBE_COMPLETED).await?;
    let concurrency = concurrency.max(1);
    tracing::info!(
        concurrency,
        "distill consuming scribe.completed (durable: {})",
        specs::SCRIBE_COMPLETED.durable_name
    );

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let handler = Arc::new(handler);
    let in_flight = hs_common::event_bus::InFlight::default();

    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Error>();
    let mut delivery_error = None;
    loop {
        let next = tokio::select! {
            biased;
            Some(fatal) = fatal_rx.recv() => return Err(fatal_stop(fatal)),
            _ = &mut shutdown => {
                orderly_stop(&sem, concurrency, drain_timeout, &in_flight).await;
                return Ok(());
            }
            next = stream.next() => match next {
                Some(n) => n,
                None => break,
            },
        };
        let event = match next {
            Ok(event) => event,
            Err(e) => {
                delivery_error = Some(e);
                break;
            }
        };
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

        let permit = tokio::select! {
            biased;
            Some(fatal) = fatal_rx.recv() => {
                // Not yet started: hand the event back, delayed so the same
                // event is not first in line the instant the watcher restarts.
                nak_unstarted(&event, Some(NAK_BACKOFF)).await;
                return Err(fatal_stop(fatal));
            }
            _ = &mut shutdown => {
                nak_unstarted(&event, None).await;
                orderly_stop(&sem, concurrency, drain_timeout, &in_flight).await;
                return Ok(());
            }
            permit = sem.clone().acquire_owned() => match permit {
                Ok(p) => p,
                Err(_) => {
                    nak_unstarted(&event, None).await;
                    return Err(anyhow::anyhow!("distill worker semaphore closed"));
                }
            }
        };
        let handler = Arc::clone(&handler);
        let fatal_tx = fatal_tx.clone();
        let tracked = in_flight.track(parsed.key.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _tracked = tracked;
            let key = parsed.key.clone();
            tracing::info!(key = %key, "distill received completed event");
            // A handler that panics terminates its event: a panic is a bug
            // this input triggers, redelivery would hit it again (up to
            // `max_deliver` times, each `ack_wait` apart), and without the
            // guard the event sat un-acked until `ack_wait` expired. The
            // guard wraps the call too, so a panic before the handler's
            // first await is caught as well.
            let result =
                match hs_common::panic_guard::catch_panic(async move { handler(parsed).await })
                    .await
                {
                    Ok(result) => result,
                    Err(panic) => {
                        tracing::error!(
                            key = %key,
                            panic = %panic,
                            "distill handler PANICKED — terminating this event (will not redeliver)"
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
                Err(HandlerError::Fatal(e)) => {
                    tracing::error!(
                        key = %key,
                        error = ?e,
                        "distill server rejected our credentials: HS_BACKEND_TOKEN is missing or \
                         does not match the server's — stopping the watcher"
                    );
                    // Leave the event for the next, correctly configured run,
                    // delayed so it is not first in line on every restart.
                    if let Err(ne) = event.nak(Some(NAK_BACKOFF)).await {
                        tracing::warn!(key = %key, error = %ne, "nak failed");
                    }
                    let _ = fatal_tx.send(e);
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

    // The stream ends (or yields a delivery error) when the broker dropped
    // the consumer or the connection (a competing watcher deleting the
    // durable, a broker restart, missed heartbeats). Consumption has
    // stopped, so this is a failure — returning Ok would let the process
    // exit 0 and stay down. Let handlers that are already running finish
    // their ack/nak, within the drain timeout; the rest are abandoned and
    // named in an ERROR log (their events are redelivered after ack_wait).
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
    Err(match delivery_error {
        Some(e) => e.context(format!(
            "event delivery for {} failed: the consumer is no longer receiving{abandoned_note}",
            specs::SCRIBE_COMPLETED.subject
        )),
        None => anyhow::anyhow!(
            "event stream ended: the consumer or broker connection for {} is gone{abandoned_note}",
            specs::SCRIBE_COMPLETED.subject
        ),
    })
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
        /// When set, the stream yields this delivery error after the events.
        then_fail: Mutex<Option<String>>,
        /// Keep the stream open (pending) after the events instead of ending it.
        keep_open: AtomicBool,
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
            let failure = self.then_fail.lock().take();
            let items = events
                .into_iter()
                .map(Ok)
                .chain(failure.map(|m| Err(anyhow::anyhow!(m))));
            let items = futures_util::stream::iter(items);
            if self.keep_open.load(Ordering::SeqCst) {
                Ok(Box::pin(items.chain(futures_util::stream::pending())))
            } else {
                Ok(Box::pin(items))
            }
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
            Err(HandlerError::Fatal(_)) => "fatal",
        }
    }

    #[tokio::test]
    async fn an_index_panic_is_permanent_and_rejected_credentials_are_fatal() {
        let server = serve(|_| Reply::Json(500, "index_panicked: x".into())).await;
        let panicked = serve_with_code().await;
        let mut rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        rig.client = DistillClient::new(&panicked.url()).unwrap();
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "permanent");
        // A plain 500 without the code stays transient.
        rig.client = DistillClient::new(&server.url()).unwrap();
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "transient");
        // A 401 naming the distill realm is a configuration error; a bare
        // 401 or any 403 (proxy/gateway) is an ordinary transient failure.
        let auth = serve(|_| {
            Reply::JsonWithHeader(
                401,
                "unauthorized".into(),
                "www-authenticate",
                "Bearer realm=\"hs-distill\"",
            )
        })
        .await;
        rig.client = DistillClient::new(&auth.url()).unwrap();
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "fatal");
        assert!(rig.bus.published.lock().is_empty());
        for status in [401u16, 403] {
            let proxy = serve(move |_| Reply::Json(status, "denied".into())).await;
            rig.client = DistillClient::new(&proxy.url()).unwrap();
            assert_eq!(
                kind_of(run(&rig, "markdown/do/doc.md").await),
                "transient",
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn rejected_credentials_stamp_nothing_on_the_catalog() {
        let rig0 = rig(|| Reply::Json(200, OK_INDEX.into())).await;
        let auth = serve(|_| {
            Reply::JsonWithHeader(
                401,
                "unauthorized".into(),
                "www-authenticate",
                "Bearer realm=\"hs-distill\"",
            )
        })
        .await;
        let mut rig = rig0;
        rig.client = DistillClient::new(&auth.url()).unwrap();
        assert_eq!(kind_of(run(&rig, "markdown/do/doc.md").await), "fatal");
        assert!(
            hs_common::catalog::read_catalog_entry_via(&rig.storage, "catalog", "doc")
                .await
                .unwrap()
                .is_none(),
            "a configuration error is not a verdict on the document"
        );
    }

    #[tokio::test]
    async fn preflight_stops_on_rejected_credentials_only() {
        let auth = serve(|_| {
            Reply::JsonWithHeader(
                401,
                "no".into(),
                "www-authenticate",
                "Bearer realm=\"hs-distill\"",
            )
        })
        .await;
        let err = preflight(&DistillClient::new(&auth.url()).unwrap())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"), "{err:#}");
        assert_eq!(auth.recorded().len(), 1);
        assert!(auth.recorded()[0].request_line.starts_with("GET /status"));

        let down = serve(|_| Reply::Json(503, "starting".into())).await;
        preflight(&DistillClient::new(&down.url()).unwrap())
            .await
            .unwrap();
        let ok = serve(|_| {
            Reply::Json(
                200,
                r#"{"collection":"c","points_count":0,"compute_device":"Cuda"}"#.into(),
            )
        })
        .await;
        preflight(&DistillClient::new(&ok.url()).unwrap())
            .await
            .unwrap();
    }

    /// Raw loopback server answering 500 with the panic header.
    async fn serve_with_code() -> FakeHttp {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 500 X\r\nx-hs-error-code: index_panicked\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno",
                        )
                        .await;
                    let _ = s.shutdown().await;
                });
            }
        });
        FakeHttp {
            addr,
            requests: Default::default(),
        }
    }

    #[tokio::test]
    async fn the_watcher_stops_with_an_error_naming_the_token_when_credentials_are_rejected() {
        let bus = Arc::new(FakeBus::default());
        *bus.events.lock() = vec![completed("a"), completed("b"), completed("c")];
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let seen = Arc::new(AtomicUsize::new(0));
        let seen2 = seen.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_subscriber(bus, storage, 1, Duration::from_secs(30), move |_| {
                let seen = seen2.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err(HandlerError::Fatal(anyhow::Error::new(
                        crate::client::Unauthorized {
                            status: reqwest::StatusCode::UNAUTHORIZED,
                            body: String::new(),
                        },
                    )))
                }
            }),
        )
        .await
        .expect("must not hang");
        let err = result.expect_err("fatal must end the watcher with an error");
        assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"), "{err:#}");
        assert!(
            seen.load(Ordering::SeqCst) < 3,
            "must stop consuming after the first fatal"
        );
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
    async fn a_zero_chunk_index_is_stamped_as_a_skip_not_as_embedded() {
        // The server answers Ok(0) for empty/stub/low-quality documents (and
        // has already removed any earlier chunks); the catalog must say
        // "skipped", never "embedded with 0 chunks".
        let rig = rig(|| {
            Reply::Json(
                200,
                r#"{"doc_id":"doc","chunks_indexed":0,"embedding_device":"Cuda"}"#.into(),
            )
        })
        .await;
        run(&rig, "markdown/do/doc.md")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let row = hs_common::catalog::read_catalog_entry_via(&rig.storage, "catalog", "doc")
            .await
            .unwrap()
            .unwrap();
        assert!(row.embedding.is_none());
        assert_eq!(row.embedding_skip.unwrap().reason, "zero_chunks_or_empty");
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
    async fn a_document_the_server_refuses_is_permanent_but_an_outage_is_not() {
        // 4xx for what the request contains (bad input, too large) will
        // never succeed; 5xx and throttling will.
        for (status, expected) in [
            (400, "permanent"),
            (413, "permanent"),
            (422, "permanent"),
            (429, "transient"),
            (500, "transient"),
            (503, "transient"),
        ] {
            let server = serve(move |_| Reply::Json(status, "no".into())).await;
            let mut rig = rig(|| Reply::Json(200, OK_INDEX.into())).await;
            rig.client = DistillClient::new(&server.url()).unwrap();
            assert_eq!(
                kind_of(run(&rig, "markdown/do/doc.md").await),
                expected,
                "HTTP {status}"
            );
        }
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
        let result = run_subscriber(
            bus,
            storage,
            concurrency,
            Duration::from_secs(30),
            move |e| {
                let seen = seen_in_handler.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    seen.lock().push(e.key);
                    Ok(())
                }
            },
        )
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

    #[tokio::test]
    async fn a_delivery_error_is_reported_with_its_cause_after_running_handlers_finish() {
        let bus = Arc::new(FakeBus::default());
        *bus.events.lock() = vec![completed("a"), completed("b")];
        *bus.then_fail.lock() = Some("missed idle heartbeat".into());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_handler = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let err = run_subscriber(bus, storage, 2, Duration::from_secs(30), move |e| {
            let seen = seen_in_handler.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                seen.lock().push(e.key);
                Ok(())
            }
        })
        .await
        .expect_err("a delivery error must fail the subscriber");
        let shown = format!("{err:#}");
        assert!(shown.contains("missed idle heartbeat"), "{shown}");
        assert!(!shown.contains("event stream ended"), "{shown}");
        let mut seen = seen.lock().clone();
        seen.sort();
        assert_eq!(seen, ["a", "b"], "no handler may be cut off");
    }

    #[tokio::test]
    async fn a_panicking_handler_terminates_its_event_and_the_subscriber_keeps_running() {
        use hs_common::event_bus::Settlement;
        let bus = Arc::new(FakeBus::default());
        let (good_a, a_log) = Event::recording("scribe.completed", br#"{"key":"a"}"#.to_vec());
        let (poison, poison_log) =
            Event::recording("scribe.completed", br#"{"key":"poison"}"#.to_vec());
        let (good_b, b_log) = Event::recording("scribe.completed", br#"{"key":"b"}"#.to_vec());
        *bus.events.lock() = vec![good_a, poison, good_b];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_handler = seen.clone();
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        // Concurrency 1: the permit the panicking task held must be released
        // or the event after it would never be dispatched.
        let result = run_subscriber(bus, storage, 1, Duration::from_secs(30), move |e| {
            let seen = seen_in_handler.clone();
            async move {
                if e.key == "poison" {
                    panic!("chunker exploded on {}", e.key);
                }
                seen.lock().push(e.key);
                Ok(())
            }
        })
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("event stream ended"));
        assert_eq!(poison_log.decisions(), [Settlement::Term]);
        assert_eq!(a_log.decisions(), [Settlement::Ack]);
        assert_eq!(b_log.decisions(), [Settlement::Ack]);
        let mut seen = seen.lock().clone();
        seen.sort();
        assert_eq!(seen, ["a", "b"]);
    }

    #[tokio::test]
    async fn a_handler_that_never_finishes_is_abandoned_after_the_drain_timeout_and_named() {
        use hs_common::event_bus::Settlement;
        let bus = Arc::new(FakeBus::default());
        let (stuck, stuck_log) =
            Event::recording("scribe.completed", br#"{"key":"stuck"}"#.to_vec());
        let (quick, quick_log) =
            Event::recording("scribe.completed", br#"{"key":"quick"}"#.to_vec());
        *bus.events.lock() = vec![stuck, quick];
        *bus.then_fail.lock() = Some("missed idle heartbeat".into());
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let started = std::time::Instant::now();
        let err = run_subscriber(
            bus,
            storage,
            2,
            Duration::from_millis(300),
            move |e| async move {
                if e.key == "stuck" {
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
        // The finished one was acked; the abandoned one is left un-acked for
        // the broker to redeliver after ack_wait.
        assert_eq!(quick_log.decisions(), [Settlement::Ack]);
        assert!(stuck_log.decisions().is_empty());
    }

    // ── R4: unstarted events go back to the broker ─────────────────────

    use hs_common::event_bus::Settlement;

    /// concurrency 1: event "a" occupies the only permit (blocked), event "b"
    /// is pulled and waits for a permit. `trigger` then ends the run.
    async fn held_event_scenario(fatal_on_a: bool) -> (anyhow::Result<()>, Vec<Settlement>) {
        let bus = Arc::new(FakeBus::default());
        bus.keep_open.store(true, Ordering::SeqCst);
        let (ea, _la) = Event::recording("scribe.completed", br#"{"key":"a"}"#.to_vec());
        let (eb, log_b) = Event::recording("scribe.completed", br#"{"key":"b"}"#.to_vec());
        *bus.events.lock() = vec![ea, eb];
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let started = Arc::new(tokio::sync::Notify::new());
        let started2 = started.clone();
        let stop_tx = Mutex::new(Some(stop_tx));
        let run = tokio::spawn(run_subscriber_until(
            bus,
            storage,
            1,
            Duration::from_millis(200),
            move |e| {
                let started = started2.clone();
                async move {
                    assert_eq!(e.key, "a", "b must never start");
                    started.notify_one();
                    if fatal_on_a {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Err(HandlerError::Fatal(anyhow::anyhow!("rejected")))
                    } else {
                        std::future::pending::<()>().await;
                        Ok(())
                    }
                }
            },
            async move {
                let _ = stop_rx.await;
            },
        ));
        started.notified().await;
        tokio::time::sleep(Duration::from_millis(100)).await; // b is now held
        if !fatal_on_a {
            if let Some(tx) = stop_tx.lock().take() {
                let _ = tx.send(());
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("must not hang")
            .unwrap();
        (result, log_b.decisions())
    }

    #[tokio::test]
    async fn an_orderly_stop_naks_the_held_event_immediately_and_returns_ok() {
        let (result, b) = held_event_scenario(false).await;
        result.expect("an orderly stop is not an error");
        assert_eq!(b, [Settlement::Nak(None)]);
    }

    #[tokio::test]
    async fn a_fatal_error_naks_the_held_event_with_a_delay() {
        let (result, b) = held_event_scenario(true).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("HS_BACKEND_TOKEN"));
        assert_eq!(b, [Settlement::Nak(Some(NAK_BACKOFF))]);
    }

    #[tokio::test]
    async fn the_fatal_event_itself_is_naked_with_a_delay() {
        let bus = Arc::new(FakeBus::default());
        bus.keep_open.store(true, Ordering::SeqCst);
        let (ea, log_a) = Event::recording("scribe.completed", br#"{"key":"a"}"#.to_vec());
        *bus.events.lock() = vec![ea];
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(std::env::temp_dir()));
        let r = run_subscriber(bus, storage, 1, Duration::from_secs(5), |_| async {
            Err(HandlerError::Fatal(anyhow::anyhow!("rejected")))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(log_a.decisions(), [Settlement::Nak(Some(NAK_BACKOFF))]);
    }
}
