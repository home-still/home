//! Maintenance of the documents a store holds, run from the command line.
//!
//! Each command is a module with a pure `run` function, which takes the
//! components it needs as borrowed arguments, and a thin `cmd`, which builds
//! those components from the user's configuration through [`MaintainCtx`].

use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::config_file::ConfigFile;
use hs_common::event_bus::{EventBus, EventBusConfig};
use hs_common::storage::Storage;
use hs_common::stores::Stores;
use hs_distill::client::DistillClient;
use hs_distill::config::DistillClientConfig;

pub(crate) mod backfill_titles;
pub(crate) mod dedupe_url_encoded;
pub(crate) mod distill_backfill;
pub(crate) mod purge;
pub(crate) mod reindex;
pub(crate) mod repair_catalog;
pub(crate) mod scan_repetitions;
pub(crate) mod scribe_queue;

/// The user's configuration, read once, and one loader for each component a
/// maintenance command may need.
///
/// A command builds only the components it uses, so a command that reads
/// storage alone does not need an event bus or a distill server. A component
/// that is not configured is an error that names the key to set.
pub(crate) struct MaintainCtx {
    file: ConfigFile,
}

impl MaintainCtx {
    /// Read `~/.home-still/config.yaml`.
    pub(crate) fn load() -> Result<Self> {
        Ok(Self {
            file: ConfigFile::load().context("reading ~/.home-still/config.yaml")?,
        })
    }

    /// The configured storage backend, checked to be usable.
    pub(crate) async fn storage(&self) -> Result<Arc<dyn Storage>> {
        let storage = self
            .file
            .storage()
            .context("reading the `storage:` section")?
            .build()
            .context("building the storage backend")?;
        storage
            .ensure_ready()
            .await
            .context("the storage backend is not ready")?;
        Ok(storage)
    }

    /// The event bus, connected. A configuration without an `events:`
    /// section is an error.
    pub(crate) async fn events(&self) -> Result<Arc<dyn EventBus>> {
        let config =
            EventBusConfig::from_file(&self.file).context("reading the `events:` section")?;
        let bus = EventBusConfig::build_required(config.as_ref()).await?;
        bus.ensure_ready()
            .await
            .context("the event bus is not reachable")?;
        Ok(bus)
    }

    /// The store registry.
    pub(crate) fn stores(&self) -> Result<Arc<Stores>> {
        let system = self
            .file
            .load_system()
            .context("reading the system-wide config file")?;
        Ok(Arc::new(
            Stores::from_config(&system, &self.file).context("reading the store registry")?,
        ))
    }

    /// A client for `server_override`, or for the first server in
    /// `distill.servers`, with the configured indexing deadline. With no
    /// override, an empty list is an error.
    pub(crate) async fn distill(&self, server_override: Option<&str>) -> Result<DistillClient> {
        let config =
            DistillClientConfig::from_file(&self.file).context("reading the `distill:` section")?;
        let url = match server_override {
            Some(url) => url.to_string(),
            None => config
                .require_servers()?
                .first()
                .context("`distill.servers` is empty")?
                .clone(),
        };
        Ok(crate::distill_cmd::make_distill_client(&url)
            .await?
            .with_index_timeout(config.index_timeout()))
    }
}

/// Fixtures the tests of the maintenance commands share.
#[cfg(test)]
pub(crate) mod test_support {
    use hs_common::catalog::ConversionMeta;
    use hs_common::event_bus::{ConsumerSpec, EventBus, EventStream};
    use hs_common::stores::{Stores, PERSONAL_PREFIX_DEFAULT};
    use parking_lot::Mutex;

    /// The store registry with the default personal prefix.
    pub(crate) fn stores() -> Stores {
        Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT).unwrap()
    }

    /// A conversion stamp: what a converted document's catalog entry carries.
    pub(crate) fn conversion() -> ConversionMeta {
        ConversionMeta {
            server: "test".into(),
            duration_secs: 1.0,
            total_pages: 1,
            converted_at: "2026-01-01T00:00:00+00:00".into(),
            pages: Vec::new(),
            converted_by: None,
            attempts_log: Vec::new(),
        }
    }

    /// An event bus that remembers what was published, and can be made to
    /// refuse every publish.
    #[derive(Default)]
    pub(crate) struct RecordingBus {
        pub(crate) published: Mutex<Vec<(String, serde_json::Value)>>,
        pub(crate) refuse: bool,
    }

    #[async_trait::async_trait]
    impl EventBus for RecordingBus {
        async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
            if self.refuse {
                anyhow::bail!("the broker refused the publish");
            }
            self.published
                .lock()
                .push((subject.to_string(), serde_json::from_slice(payload)?));
            Ok(())
        }

        async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
            anyhow::bail!("RecordingBus only records publishes")
        }
    }
}
