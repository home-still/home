use async_trait::async_trait;
use futures::StreamExt;
use std::time::Duration;

use super::{ConsumerSpec, Event, EventBus, EventStream};

#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
    /// Per-message processing deadline. Used as the JetStream consumer's
    /// `ack_wait` — if the handler hasn't acked within this window, the
    /// broker treats the delivery as lost and redelivers. Should be a
    /// comfortable multiple of the expected handler runtime (for scribe:
    /// `convert_timeout_secs * 2`).
    pub ack_wait: Duration,
    /// After this many deliveries without an ack/term, JetStream gives
    /// up and drops the message. A malformed event that keeps NAK-ing
    /// will eventually stop blocking the queue.
    pub max_deliver: i64,
    /// How long to retain un-acked messages in the stream. Bounds the
    /// catch-up window if a worker stays down.
    pub max_age: Duration,
    /// Server-side cap on un-acked messages outstanding to this
    /// consumer group. Without this the async-nats pull consumer
    /// buffers hundreds of messages on the client; a tail message then
    /// sits un-acked for > `ack_wait` while the handler works through
    /// earlier messages, triggering spurious redeliveries. Set to
    /// roughly `2 * handler_concurrency` so the server keeps the
    /// cluster busy without over-committing.
    pub max_ack_pending: i64,
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: "nats://localhost:4222".into(),
            ack_wait: Duration::from_secs(1800),
            max_deliver: 5,
            max_age: Duration::from_secs(7 * 24 * 3600),
            max_ack_pending: 32,
        }
    }
}

/// JetStream names used by the pipeline. Paired with subjects so the
/// stream captures everything that might land on a matching subject
/// without the publisher needing to know the stream name.
const PAPERS_STREAM: &str = "PAPERS";
const PAPERS_SUBJECTS: &[&str] = &["papers.>"];
const SCRIBE_STREAM: &str = "SCRIBE";
const SCRIBE_SUBJECTS: &[&str] = &["scribe.>"];

pub struct NatsBus {
    jetstream: async_nats::jetstream::Context,
    cfg: NatsConfig,
}

impl NatsBus {
    pub async fn connect(cfg: NatsConfig) -> anyhow::Result<Self> {
        let client = async_nats::connect(&cfg.url).await?;
        let jetstream = async_nats::jetstream::new(client);
        let bus = Self { jetstream, cfg };
        // Provision both streams up-front so the first publish / consume
        // on a cold broker doesn't race. get_or_create is idempotent.
        bus.ensure_stream(PAPERS_STREAM, PAPERS_SUBJECTS).await?;
        bus.ensure_stream(SCRIBE_STREAM, SCRIBE_SUBJECTS).await?;
        Ok(bus)
    }

    async fn ensure_stream(&self, name: &str, subjects: &[&str]) -> anyhow::Result<()> {
        use async_nats::jetstream::stream::{Config, DiscardPolicy, RetentionPolicy, StorageType};
        // WorkQueue: message is removed after the first successful ack.
        // That's exactly the semantics we want — once scribe converts a
        // paper, the `papers.ingested` for it is gone. DiscardPolicy::Old
        // means on a full stream we discard the oldest undelivered
        // message; in WorkQueue mode that's the only valid policy.
        let config = Config {
            name: name.to_string(),
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
            retention: RetentionPolicy::WorkQueue,
            storage: StorageType::File,
            discard: DiscardPolicy::Old,
            max_age: self.cfg.max_age,
            ..Default::default()
        };
        self.jetstream
            .get_or_create_stream(config)
            .await
            .map_err(|e| anyhow::anyhow!("create stream {name}: {e}"))?;
        Ok(())
    }

    async fn ensure_consumer(
        &self,
        spec: &ConsumerSpec,
    ) -> anyhow::Result<
        async_nats::jetstream::consumer::Consumer<async_nats::jetstream::consumer::pull::Config>,
    > {
        use async_nats::jetstream::consumer::{pull::Config as PullConfig, AckPolicy};
        let stream = self
            .jetstream
            .get_stream(spec.stream)
            .await
            .map_err(|e| anyhow::anyhow!("get stream {}: {e}", spec.stream))?;
        let config = PullConfig {
            durable_name: Some(spec.durable_name.to_string()),
            filter_subject: spec.subject.to_string(),
            ack_policy: AckPolicy::Explicit,
            ack_wait: self.cfg.ack_wait,
            max_deliver: self.cfg.max_deliver,
            max_ack_pending: self.cfg.max_ack_pending,
            ..Default::default()
        };
        // async_nats 0.47's `get_or_create_consumer` silently keeps
        // the existing config on mismatch — a stale test consumer
        // once pinned production to ack_wait=10s. Delete first (no-op
        // if absent), then create fresh from the current NatsConfig.
        // Single-daemon-per-consumer-group means no race.
        if let Err(e) = stream.delete_consumer(spec.durable_name).await {
            let msg = format!("{e}");
            if !msg.contains("not found") && !msg.contains("10014") {
                tracing::warn!(
                    consumer = spec.durable_name,
                    error = %e,
                    "delete_consumer before recreate failed; continuing"
                );
            }
        }
        stream
            .create_consumer(config)
            .await
            .map_err(|e| anyhow::anyhow!("create consumer {}: {e}", spec.durable_name))
    }

    /// Delete the two pipeline streams (PAPERS, SCRIBE). All queued and
    /// in-flight messages are discarded. Operators use this to recover
    /// from config drift (e.g. a consumer stuck with the wrong ack_wait)
    /// — after wiping, the next [`connect`] recreates everything from
    /// the current [`NatsConfig`].
    pub async fn reset_streams(&self) -> anyhow::Result<()> {
        for stream in [PAPERS_STREAM, SCRIBE_STREAM] {
            match self.jetstream.delete_stream(stream).await {
                Ok(_) => tracing::info!(stream, "deleted jetstream stream"),
                Err(e) => {
                    // Not-found is OK — operator may be running reset
                    // on a cold broker.
                    let msg = format!("{e}");
                    if msg.contains("not found") {
                        tracing::info!(stream, "stream absent (nothing to delete)");
                    } else {
                        return Err(anyhow::anyhow!("delete stream {stream}: {e}"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EventBus for NatsBus {
    async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
        // JetStream publish blocks until the server persists the
        // message and returns a PublishAck. `.await` on the returned
        // future waits for that confirmation — losing that await would
        // re-introduce the at-most-once "lie" that the previous core
        // NATS impl was reverting away from.
        let ack = self
            .jetstream
            .publish(subject.to_string(), bytes::Bytes::copy_from_slice(payload))
            .await
            .map_err(|e| anyhow::anyhow!("jetstream publish {subject}: {e}"))?;
        ack.await
            .map_err(|e| anyhow::anyhow!("jetstream publish ack {subject}: {e}"))?;
        Ok(())
    }

    async fn consume(&self, spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
        let consumer = self.ensure_consumer(spec).await?;
        let messages = consumer
            .messages()
            .await
            .map_err(|e| anyhow::anyhow!("consumer.messages(): {e}"))?;
        // Drop delivery errors (e.g. heartbeat missed mid-stream) with a
        // warning; JetStream will redeliver unacked messages anyway.
        let stream = messages.filter_map(|m| async move {
            match m {
                Ok(msg) => Some(Event::from_jetstream(msg)),
                Err(e) => {
                    tracing::warn!(error = %e, "jetstream delivery error");
                    None
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::specs;
    use futures::StreamExt;

    fn nats_url() -> Option<String> {
        std::env::var("HS_NATS_URL").ok()
    }

    #[tokio::test]
    async fn nats_publish_consume_roundtrip() {
        let Some(url) = nats_url() else {
            eprintln!("skipping: set HS_NATS_URL to run");
            return;
        };
        let bus = NatsBus::connect(NatsConfig {
            url,
            ack_wait: Duration::from_secs(10),
            max_deliver: 3,
            max_age: Duration::from_secs(3600),
            max_ack_pending: 8,
        })
        .await
        .unwrap();

        let mut stream = bus.consume(&specs::PAPERS_INGESTED).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        bus.publish("papers.ingested", b"hello-jetstream")
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timed out waiting for event")
            .expect("stream closed");
        assert_eq!(got.subject, "papers.ingested");
        assert_eq!(got.payload, b"hello-jetstream");
        got.ack().await.unwrap();
    }
}
