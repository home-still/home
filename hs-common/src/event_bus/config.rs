use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{EventBus, NoOpBus};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventsBackend {
    /// Drop publishes, never yield events. Keeps code paths compatible while
    /// running in legacy filesystem-watcher mode.
    #[default]
    Noop,
    /// Connect to a NATS server (requires `events-nats` cargo feature).
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
}

impl Default for NatsYaml {
    fn default() -> Self {
        Self {
            url: "nats://localhost:4222".into(),
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
                    })
                    .await?;
                    Ok(Arc::new(bus))
                }
                #[cfg(not(feature = "events-nats"))]
                {
                    anyhow::bail!(
                        "events.backend=nats requires the `events-nats` cargo feature"
                    );
                }
            }
        }
    }
}
