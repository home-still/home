use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
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
    /// Boxed to keep `Event` small — `jetstream::Message` carries the
    /// full received payload and metadata (~400 B).
    #[cfg(feature = "events-nats")]
    JetStream(Box<async_nats::jetstream::Message>),
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

    /// Acknowledge successful processing. For JetStream, removes the
    /// message from the work queue. For non-durable buses, no-op.
    pub async fn ack(&self) -> anyhow::Result<()> {
        match &self.handle {
            AckHandle::None => Ok(()),
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

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

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
    /// must be explicitly acked/naked/termed by the caller.
    async fn consume(&self, spec: &ConsumerSpec) -> anyhow::Result<EventStream>;
}

/// A bus that silently drops publishes and produces no events. Useful as a
/// default during migration and in tests.
pub struct NoOpBus;

#[async_trait]
impl EventBus for NoOpBus {
    async fn publish(&self, _subject: &str, _payload: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
        Ok(Box::pin(futures::stream::pending()))
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
    use futures::StreamExt;
    use std::time::Duration;

    #[tokio::test]
    async fn noop_publish_ok_consume_pending() {
        let bus = NoOpBus;
        bus.publish("x.y", b"hi").await.unwrap();

        let mut stream = bus.consume(&specs::PAPERS_INGESTED).await.unwrap();
        let got = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(got.is_err(), "NoOpBus consume should never yield");
    }

    #[tokio::test]
    async fn inert_event_ack_nak_term_are_noops() {
        let ev = Event::inert("x", b"y".to_vec());
        ev.ack().await.unwrap();
        ev.nak(Some(Duration::from_secs(1))).await.unwrap();
        ev.term().await.unwrap();
    }
}
