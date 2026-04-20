use async_trait::async_trait;
use futures::StreamExt;

use super::{Event, EventBus, EventStream};

#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
}

pub struct NatsBus {
    client: async_nats::Client,
}

impl NatsBus {
    pub async fn connect(cfg: NatsConfig) -> anyhow::Result<Self> {
        let client = async_nats::connect(&cfg.url).await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl EventBus for NatsBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
        self.client
            .publish(subject.to_string(), bytes::Bytes::copy_from_slice(payload))
            .await?;
        // Ensure server received before returning (at-most-once durability for callers).
        self.client.flush().await?;
        Ok(())
    }

    async fn subscribe(&self, subject: &str) -> anyhow::Result<EventStream> {
        let sub = self.client.subscribe(subject.to_string()).await?;
        let stream = sub.map(|m| Event {
            subject: m.subject.to_string(),
            payload: m.payload.to_vec(),
        });
        Ok(Box::pin(stream))
    }

    async fn queue_subscribe(&self, subject: &str, queue: &str) -> anyhow::Result<EventStream> {
        let sub = self
            .client
            .queue_subscribe(subject.to_string(), queue.to_string())
            .await?;
        let stream = sub.map(|m| Event {
            subject: m.subject.to_string(),
            payload: m.payload.to_vec(),
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    fn nats_url() -> Option<String> {
        std::env::var("HS_NATS_URL").ok()
    }

    #[tokio::test]
    async fn nats_pubsub_roundtrip() {
        let Some(url) = nats_url() else {
            eprintln!("skipping: set HS_NATS_URL to run");
            return;
        };
        let bus = NatsBus::connect(NatsConfig { url }).await.unwrap();

        let subject = format!("hs.test.{}", std::process::id());
        let mut sub = bus.subscribe(&subject).await.unwrap();

        // Give the server a beat to register the subscription.
        tokio::time::sleep(Duration::from_millis(50)).await;

        bus.publish(&subject, b"hello-events").await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("timed out waiting for event")
            .expect("stream closed");
        assert_eq!(got.subject, subject);
        assert_eq!(got.payload, b"hello-events");
    }
}
