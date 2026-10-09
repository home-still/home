use async_trait::async_trait;
use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "events-nats")]
pub mod nats;

#[cfg(feature = "events-nats")]
pub use nats::NatsBus;

pub mod config;
pub use config::{EventBusConfig, EventsBackend};

/// A single event delivered by [`EventBus::consume`]. Each event holds an
/// ack handle: the subscriber MUST call exactly one of [`Event::ack`],
/// [`Event::nak`], or [`Event::term`] before dropping it. Dropping an
/// un-acked event lets the server redeliver it after `ack_wait` — fine as
/// a crash-recovery mechanism, not as a normal flow.
pub struct Event {
    pub subject: String,
    pub payload: Vec<u8>,
    handle: AckHandle,
}

enum AckHandle {
    /// No-op (tests / legacy publishers). ack/nak/term return Ok.
    None,
    /// Records the decision for a test to read back ([`Event::recording`]).
    Recording(Arc<Mutex<Vec<Settlement>>>),
    /// Boxed to keep `Event` small — `jetstream::Message` carries the
    /// full received payload and metadata (~400 B).
    #[cfg(feature = "events-nats")]
    JetStream(Box<async_nats::jetstream::Message>),
}

/// What a subscriber decided for one event: see [`Event::recording`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Ack,
    Nak(Option<Duration>),
    Term,
}

/// Read-back side of [`Event::recording`].
#[derive(Debug, Clone, Default)]
pub struct SettlementLog(Arc<Mutex<Vec<Settlement>>>);

impl SettlementLog {
    /// Every decision made so far, in order.
    pub fn decisions(&self) -> Vec<Settlement> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Event")
            .field("subject", &self.subject)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Event {
    /// Build an event with no ack handle. Used by [`NoOpBus`] and tests —
    /// ack/nak/term all return Ok.
    pub fn inert(subject: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            subject: subject.into(),
            payload: payload.into(),
            handle: AckHandle::None,
        }
    }

    /// Build an event whose ack/nak/term decision is recorded in the returned
    /// log instead of sent anywhere. For tests of subscriber loops: it lets a
    /// test assert *what was decided* for an event (for example that a
    /// handler panic terminated it) without a broker.
    pub fn recording(
        subject: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> (Self, SettlementLog) {
        let log = SettlementLog::default();
        let event = Self {
            subject: subject.into(),
            payload: payload.into(),
            handle: AckHandle::Recording(log.0.clone()),
        };
        (event, log)
    }

    fn record(log: &Mutex<Vec<Settlement>>, decision: Settlement) {
        log.lock().unwrap_or_else(|e| e.into_inner()).push(decision);
    }

    /// Acknowledge successful processing. For JetStream, removes the
    /// message from the work queue. For non-durable buses, no-op.
    pub async fn ack(&self) -> anyhow::Result<()> {
        match &self.handle {
            AckHandle::None => Ok(()),
            AckHandle::Recording(log) => {
                Self::record(log, Settlement::Ack);
                Ok(())
            }
            #[cfg(feature = "events-nats")]
            AckHandle::JetStream(m) => m
                .ack()
                .await
                .map_err(|e| anyhow::anyhow!("jetstream ack: {e}")),
        }
    }

    /// Negative-ack: redeliver after `delay` (or immediately if `None`).
    /// Use for transient failures (downstream server saturated, network
    /// blip, etc.). JetStream counts this against `max_deliver`.
    pub async fn nak(&self, delay: Option<Duration>) -> anyhow::Result<()> {
        match &self.handle {
            AckHandle::None => Ok(()),
            AckHandle::Recording(log) => {
                Self::record(log, Settlement::Nak(delay));
                Ok(())
            }
            #[cfg(feature = "events-nats")]
            AckHandle::JetStream(m) => m
                .ack_with(async_nats::jetstream::AckKind::Nak(delay))
                .await
                .map_err(|e| anyhow::anyhow!("jetstream nak: {e}")),
        }
    }

    /// Terminal reject: do not redeliver, ever. Use for permanent
    /// failures (malformed payload, VLM repetition loop, unsupported
    /// file type) so bad input doesn't cycle forever.
    pub async fn term(&self) -> anyhow::Result<()> {
        match &self.handle {
            AckHandle::None => Ok(()),
            AckHandle::Recording(log) => {
                Self::record(log, Settlement::Term);
                Ok(())
            }
            #[cfg(feature = "events-nats")]
            AckHandle::JetStream(m) => m
                .ack_with(async_nats::jetstream::AckKind::Term)
                .await
                .map_err(|e| anyhow::anyhow!("jetstream term: {e}")),
        }
    }
}

/// The only place where an [`Event`] with a JetStream handle is
/// constructed outside this module. Publicly re-exported via
/// `pub use event_bus::nats` consumers.
#[cfg(feature = "events-nats")]
impl Event {
    pub(crate) fn from_jetstream(m: async_nats::jetstream::Message) -> Self {
        let subject = m.subject.to_string();
        let payload = m.payload.to_vec();
        Self {
            subject,
            payload,
            handle: AckHandle::JetStream(Box::new(m)),
        }
    }
}

/// What [`EventBus::consume`] yields. An `Err` item means delivery is
/// broken (the consumer was deleted, the connection dropped, the broker
/// answered with an error) and is the last item: the subscriber must stop,
/// finish the handlers it already started, and fail, so its supervisor
/// restarts it on a fresh consumer. `None` (the stream ending without an
/// error) also means consumption has stopped.
pub type EventStream = Pin<Box<dyn Stream<Item = anyhow::Result<Event>> + Send>>;

/// Identifier of a logical subscribe target. A subject selector plus a
/// durable consumer name. Multiple processes sharing the same
/// `durable_name` split work (JetStream's pull-consumer equivalent of a
/// queue group).
#[derive(Debug, Clone)]
pub struct ConsumerSpec {
    /// Stream the consumer binds to. Currently the canonical stream
    /// names are `PAPERS` (subjects `papers.>`) and `SCRIBE`
    /// (subjects `scribe.>`).
    pub stream: &'static str,
    /// Subject to filter deliveries on, e.g. `papers.ingested`.
    pub subject: &'static str,
    /// Durable consumer name. Processes sharing this name load-balance
    /// the subject's messages between them.
    pub durable_name: &'static str,
}

/// Outstanding work on one durable consumer, as the broker counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDepth {
    /// Messages in the stream not yet delivered to any worker.
    pub pending: u64,
    /// Messages delivered to a worker and not yet acked: being converted,
    /// or awaiting redelivery after `ack_wait`.
    pub ack_pending: u64,
}

impl QueueDepth {
    /// Everything the consumer has not finished: waiting plus being worked.
    pub fn outstanding(&self) -> u64 {
        self.pending.saturating_add(self.ack_pending)
    }
}

#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publish a payload on `subject`. JetStream buses persist the
    /// message in the corresponding stream; the publisher blocks until
    /// the broker confirms receipt.
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()>;

    /// Pull-consume messages matching `spec`. Each yielded [`Event`]
    /// must be explicitly acked/naked/termed by the caller. See
    /// [`EventStream`] for how delivery failures are reported.
    async fn consume(&self, spec: &ConsumerSpec) -> anyhow::Result<EventStream>;

    /// Fail now if this bus cannot be used. A bus that connects on first use
    /// ([`LazyBus`]) establishes its connection here, so a caller that is
    /// about to do expensive work for an event it must then publish can
    /// refuse before starting. Buses that are connected when built have
    /// nothing to check.
    async fn ensure_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// How much work is outstanding on the durable consumer `spec` names,
    /// for the status panel. Errors when the bus cannot say (no broker, no
    /// such consumer yet); an unknown depth is never reported as zero.
    async fn queue_depth(&self, spec: &ConsumerSpec) -> anyhow::Result<QueueDepth> {
        anyhow::bail!(
            "this event bus does not report queue depth (consumer {})",
            spec.durable_name
        )
    }
}

/// What a [`LazyBus`] connect function returns.
pub type BusFuture = Pin<Box<dyn Future<Output = anyhow::Result<Arc<dyn EventBus>>> + Send>>;

/// A bus whose connection is made on first use rather than at startup.
///
/// For processes that only occasionally publish (the MCP server: two tools
/// publish, a dozen read-only ones do not), so that a broker outage costs
/// those tools and nothing else instead of the whole process. The
/// configuration is still validated when the bus is built
/// ([`EventBusConfig::build_lazy`]); only the network connection is deferred.
/// A failed connection is not remembered: the next call tries again, and
/// once connected the client library reconnects by itself.
pub struct LazyBus {
    connect: Box<dyn Fn() -> BusFuture + Send + Sync>,
    bus: tokio::sync::OnceCell<Arc<dyn EventBus>>,
}

impl LazyBus {
    pub fn new(connect: impl Fn() -> BusFuture + Send + Sync + 'static) -> Self {
        Self {
            connect: Box::new(connect),
            bus: tokio::sync::OnceCell::new(),
        }
    }

    async fn bus(&self) -> anyhow::Result<&Arc<dyn EventBus>> {
        self.bus.get_or_try_init(|| (self.connect)()).await
    }
}

#[async_trait]
impl EventBus for LazyBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
        self.bus().await?.publish(subject, payload).await
    }

    async fn consume(&self, spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
        self.bus().await?.consume(spec).await
    }

    async fn ensure_ready(&self) -> anyhow::Result<()> {
        self.bus().await.map(|_| ())
    }

    async fn queue_depth(&self, spec: &ConsumerSpec) -> anyhow::Result<QueueDepth> {
        self.bus().await?.queue_depth(spec).await
    }
}

/// The keys of the handlers a subscriber has started and not finished, so
/// that a bounded drain can name what it abandons.
#[derive(Clone, Default)]
pub struct InFlight(Arc<Mutex<Vec<String>>>);

/// Removes its key from [`InFlight`] when dropped (handler finished, or its
/// task was cancelled or panicked).
pub struct InFlightGuard {
    set: InFlight,
    key: String,
}

impl InFlight {
    pub fn track(&self, key: impl Into<String>) -> InFlightGuard {
        let key = key.into();
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(key.clone());
        InFlightGuard {
            set: self.clone(),
            key,
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut keys = self.set.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = keys.iter().position(|k| *k == self.key) {
            keys.swap_remove(at);
        }
    }
}

/// After a subscriber stopped consuming (delivery error, ended stream), wait
/// up to `timeout` for the handlers it already started to finish their
/// ack/nak, then give up on the rest. Returns the keys it gave up on (empty
/// when everything finished) after logging them at ERROR: their events stay
/// un-acked and the broker redelivers them after `ack_wait`, which beats a
/// watcher that sits dead for as long as a book-length conversion.
///
/// `sem` is the dispatch semaphore (`concurrency` permits, one held per
/// running handler).
pub async fn drain_in_flight(
    sem: &tokio::sync::Semaphore,
    concurrency: usize,
    timeout: Duration,
    in_flight: &InFlight,
) -> Vec<String> {
    let all = u32::try_from(concurrency).unwrap_or(u32::MAX);
    match tokio::time::timeout(timeout, sem.acquire_many(all)).await {
        Ok(_) => Vec::new(),
        Err(_) => {
            let abandoned = in_flight.keys();
            tracing::error!(
                abandoned = ?abandoned,
                drain_timeout_secs = timeout.as_secs(),
                "event subscriber stopped; these handlers did not finish within the drain \
                 timeout and are abandoned (their events stay un-acked and are redelivered \
                 after ack_wait)"
            );
            abandoned
        }
    }
}

/// A bus that drops every publish and delivers nothing, selected by an
/// explicit `events.backend: noop` (and by tests). It is never a default:
/// see [`EventBusConfig::build_required`]. Publishing is accepted and
/// discarded on purpose; consuming is refused, because a consumer on a bus
/// that never delivers would run forever doing nothing.
pub struct NoOpBus;

#[async_trait]
impl EventBus for NoOpBus {
    async fn publish(&self, _subject: &str, _payload: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn consume(&self, spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
        anyhow::bail!(
            "events.backend is `noop`: it never delivers {}; set events.backend to `nats` to \
             consume events",
            spec.subject
        )
    }
}

/// Canonical consumer specs used across the pipeline. Keep these in one
/// place so stream and durable names stay consistent between publisher
/// stream-creation and consumer subscription.
pub mod specs {
    use super::ConsumerSpec;

    pub const PAPERS_INGESTED: ConsumerSpec = ConsumerSpec {
        stream: "PAPERS",
        subject: "papers.ingested",
        durable_name: "scribe-workers",
    };

    pub const SCRIBE_COMPLETED: ConsumerSpec = ConsumerSpec {
        stream: "SCRIBE",
        subject: "scribe.completed",
        durable_name: "distill-workers",
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn noop_accepts_publishes_and_refuses_to_be_consumed() {
        let bus = NoOpBus;
        bus.publish("x.y", b"hi").await.unwrap();

        let err = bus
            .consume(&specs::PAPERS_INGESTED)
            .await
            .err()
            .expect("a consumer on the noop bus must fail, not idle")
            .to_string();
        assert!(err.contains("papers.ingested"), "{err}");
    }

    #[tokio::test]
    async fn inert_event_ack_nak_term_are_noops() {
        let ev = Event::inert("x", b"y".to_vec());
        ev.ack().await.unwrap();
        ev.nak(Some(Duration::from_secs(1))).await.unwrap();
        ev.term().await.unwrap();
    }

    /// A bus that records publishes, handed out by the lazy connect function.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    #[async_trait]
    impl EventBus for Recording {
        async fn publish(&self, subject: &str, _payload: &[u8]) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(subject.to_string());
            Ok(())
        }
        async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
            anyhow::bail!("not consumable")
        }
    }

    #[tokio::test]
    async fn a_lazy_bus_does_not_connect_until_used_and_retries_after_a_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let bus = LazyBus::new(move || {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n < 2 {
                    anyhow::bail!("broker unreachable (attempt {n})");
                }
                Ok(Arc::new(Recording::default()) as Arc<dyn EventBus>)
            })
        });
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "building connects nothing"
        );

        // Down: every use is an error carrying the cause, and none is cached.
        let err = bus.publish("papers.ingested", b"x").await.unwrap_err();
        assert!(err.to_string().contains("broker unreachable"), "{err}");
        assert!(bus.ensure_ready().await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        // Up: the next use connects, and later uses share that connection.
        bus.ensure_ready().await.unwrap();
        bus.publish("papers.ingested", b"x").await.unwrap();
        bus.publish("scribe.completed", b"y").await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "connected exactly once");
    }

    #[tokio::test]
    async fn the_drain_returns_at_once_when_nothing_is_running() {
        let sem = tokio::sync::Semaphore::new(2);
        let in_flight = InFlight::default();
        let started = std::time::Instant::now();
        let abandoned = drain_in_flight(&sem, 2, Duration::from_secs(30), &in_flight).await;
        assert!(abandoned.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_drain_gives_up_after_its_timeout_and_names_what_it_abandoned() {
        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let in_flight = InFlight::default();
        // One handler that finishes soon, one that never does.
        for (key, runs) in [
            ("quick.pdf", Some(Duration::from_millis(30))),
            ("stuck.pdf", None),
        ] {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let guard = in_flight.track(key);
            tokio::spawn(async move {
                let _held = (permit, guard);
                match runs {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            });
        }
        let started = std::time::Instant::now();
        let abandoned = drain_in_flight(&sem, 2, Duration::from_millis(300), &in_flight).await;
        assert_eq!(abandoned, ["stuck.pdf"], "only the stuck handler is named");
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
    }

    #[test]
    fn a_finished_handler_leaves_the_in_flight_set() {
        let in_flight = InFlight::default();
        let a = in_flight.track("a");
        let b = in_flight.track("b");
        drop(a);
        assert_eq!(in_flight.keys(), ["b"]);
        drop(b);
        assert!(in_flight.keys().is_empty());
    }
}
