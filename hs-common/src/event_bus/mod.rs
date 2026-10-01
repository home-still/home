use async_trait::async_trait;
use futures::Stream;
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
}
