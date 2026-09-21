use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use super::{EventBus, NoOpBus};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventsBackend {
    /// Drop publishes, never yield events. Useful in tests and on hosts
    /// that don't participate in the event pipeline.
    #[default]
    Noop,
    /// Connect to a NATS server with JetStream (requires `events-nats`
    /// cargo feature).
    Nats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EventBusConfig {
    pub backend: EventsBackend,
    pub nats: NatsYaml,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NatsYaml {
    pub url: String,
    /// Per-message processing deadline in seconds. Translates to the
    /// JetStream consumer's `ack_wait`. Default 1800 (30 min) matches
    /// the worst-case scribe convert + slack.
    pub ack_wait_secs: u64,
    /// After this many redeliveries, JetStream drops the message.
    /// Prevents a NAK storm on a poison message from stalling the queue.
    pub max_deliver: i64,
    /// How long JetStream retains un-acked messages. Bounds the
    /// catch-up window if a worker is down.
    pub max_age_secs: u64,
    /// Upper bound on in-flight deliveries to the consumer group.
    /// Should be about 2× handler concurrency so the server keeps the
    /// pipeline busy without pre-buffering so many messages that the
    /// tail ones time out before the handler reaches them.
    pub max_ack_pending: i64,
}

impl Default for NatsYaml {
    fn default() -> Self {
        Self {
            url: "nats://localhost:4222".into(),
            // Must be ≥ the scribe timeout policy's `ceiling_secs`
            // (3600s) so a legitimately slow 500-page book doesn't get
            // reclaimed by the broker mid-convert. Set to 2× ceiling
            // for headroom — an in-flight book that stalls past 3600s
            // should still survive a broker heartbeat blip.
            ack_wait_secs: 7200,
            max_deliver: 5,
            max_age_secs: 7 * 24 * 3600,
            max_ack_pending: 32,
        }
    }
}

impl EventBusConfig {
    pub async fn build(&self) -> anyhow::Result<Arc<dyn EventBus>> {
        match self.backend {
            EventsBackend::Noop => Ok(Arc::new(NoOpBus)),
            EventsBackend::Nats => {
                #[cfg(feature = "events-nats")]
                {
                    let bus = super::nats::NatsBus::connect(super::nats::NatsConfig {
                        url: self.nats.url.clone(),
                        ack_wait: Duration::from_secs(self.nats.ack_wait_secs),
                        max_deliver: self.nats.max_deliver,
                        max_age: Duration::from_secs(self.nats.max_age_secs),
                        max_ack_pending: self.nats.max_ack_pending,
                    })
                    .await?;
                    Ok(Arc::new(bus))
                }
                #[cfg(not(feature = "events-nats"))]
                {
                    let _ = Duration::from_secs(0);
                    anyhow::bail!("events.backend=nats requires the `events-nats` cargo feature");
                }
            }
        }
    }
}
