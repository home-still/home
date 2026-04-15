use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

#[cfg(feature = "events-nats")]
pub mod nats;

#[cfg(feature = "events-nats")]
pub use nats::NatsBus;

pub mod config;
pub use config::{EventBusConfig, EventsBackend};

/// A single event received on a subscribed subject.
#[derive(Debug, Clone)]
pub struct Event {
    pub subject: String,
    pub payload: Vec<u8>,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()>;
    async fn subscribe(&self, subject: &str) -> anyhow::Result<EventStream>;
}

/// A bus that silently drops publishes and produces no events. Useful as a
/// default during migration and in tests.
pub struct NoOpBus;

#[async_trait]
impl EventBus for NoOpBus {
    async fn publish(&self, _subject: &str, _payload: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn subscribe(&self, _subject: &str) -> anyhow::Result<EventStream> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    #[tokio::test]
    async fn noop_publish_ok_subscribe_pending() {
        let bus = NoOpBus;
        bus.publish("x.y", b"hi").await.unwrap();

        let mut stream = bus.subscribe("x.y").await.unwrap();
        let got = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(got.is_err(), "NoOpBus subscribe should never yield");
    }
}
