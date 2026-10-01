use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use super::{EventBus, NoOpBus};
use crate::config_file::{ConfigError, ConfigFile};

/// Which event bus the `events:` section selects. There is no default: a
/// config without `events.backend` cannot run any component that publishes
/// or consumes events (see [`EventBusConfig::build_required`]), because a
/// silently defaulted bus drops every publish and delivers nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventsBackend {
    /// Explicitly drop publishes. Consuming from it is an error: a consumer
    /// on a bus that never delivers would sit idle and report success.
    Noop,
    /// Connect to a NATS server with JetStream (requires `events-nats`
    /// cargo feature).
    Nats,
}

/// The `events:` section. `backend` is required; unknown keys are errors
/// (a typo in an auth key must not become an unauthenticated connection).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBusConfig {
    pub backend: EventsBackend,
    #[serde(default)]
    pub nats: NatsYaml,
}

/// `events.nats`. Every key is optional; the connection keys at the bottom
/// (`credentials_file` … `require_tls`) default to "no auth, no extra TLS".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NatsYaml {
    pub url: String,
    /// Per-message processing deadline in seconds. Translates to the
    /// JetStream consumer's `ack_wait`. Default 7200 (2× the scribe
    /// timeout ceiling of 3600s) so a legitimately slow book-length
    /// convert isn't reclaimed by the broker mid-flight.
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

    /// Authenticate with a NATS `.creds` file (JWT + NKey seed).
    pub credentials_file: Option<PathBuf>,
    /// Authenticate with a token. Names the environment variable that holds
    /// it (exported by `~/.home-still/secrets.env`); the token itself never
    /// appears in the config file.
    pub token_env: Option<String>,
    /// Authenticate as this user; requires `password_env`.
    pub user: Option<String>,
    /// Names the environment variable that holds the password of `user`.
    pub password_env: Option<String>,
    /// PEM bundle of the CA that signed the broker's certificate, for a
    /// private CA the system store does not know.
    pub tls_ca_file: Option<PathBuf>,
    /// PEM client certificate for mutual TLS; requires `tls_client_key`.
    pub tls_client_cert: Option<PathBuf>,
    /// PEM private key of `tls_client_cert`.
    pub tls_client_key: Option<PathBuf>,
    /// Refuse a connection to a broker that does not offer TLS.
    pub require_tls: bool,
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
            credentials_file: None,
            token_env: None,
            user: None,
            password_env: None,
            tls_ca_file: None,
            tls_client_cert: None,
            tls_client_key: None,
            require_tls: false,
        }
    }
}

impl NatsYaml {
    /// Check the connection keys against each other. Files are checked when
    /// the connection is made (they may live on a host other than the one
    /// that parses the config).
    pub fn validate(&self) -> Result<(), String> {
        if self.url.trim().is_empty() {
            return Err("`url` must not be empty".into());
        }
        let methods = [
            self.credentials_file.is_some(),
            self.token_env.is_some(),
            self.user.is_some(),
        ];
        if methods.iter().filter(|set| **set).count() > 1 {
            return Err(
                "set only one of `credentials_file`, `token_env` and `user` + `password_env`"
                    .into(),
            );
        }
        if self.user.is_some() != self.password_env.is_some() {
            return Err("`user` and `password_env` go together".into());
        }
        if self.tls_client_cert.is_some() != self.tls_client_key.is_some() {
            return Err("`tls_client_cert` and `tls_client_key` go together".into());
        }
        for (key, value) in [
            ("token_env", &self.token_env),
            ("user", &self.user),
            ("password_env", &self.password_env),
        ] {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                return Err(format!("`{key}` must not be empty"));
            }
        }
        for (key, path) in [
            ("credentials_file", &self.credentials_file),
            ("tls_ca_file", &self.tls_ca_file),
            ("tls_client_cert", &self.tls_client_cert),
            ("tls_client_key", &self.tls_client_key),
        ] {
            if path.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
                return Err(format!("`{key}` must not be empty"));
            }
        }
        Ok(())
    }
}

impl EventBusConfig {
    /// A config selecting the `noop` backend explicitly.
    pub fn noop() -> Self {
        Self {
            backend: EventsBackend::Noop,
            nats: NatsYaml::default(),
        }
    }

    /// Check the section: the NATS keys must agree with each other.
    pub fn validate(&self) -> Result<(), String> {
        self.nats.validate().map_err(|e| format!("nats: {e}"))
    }

    /// Read and validate the `events:` section of `file`. `Ok(None)` when
    /// the section is absent: components that need a bus then refuse to
    /// start through [`Self::build_required`].
    pub fn from_file(file: &ConfigFile) -> Result<Option<Self>, ConfigError> {
        let Some(cfg) = file.section::<Self>("events")? else {
            return Ok(None);
        };
        cfg.validate()
            .map_err(|e| ConfigError::section(file.path(), "events", e))?;
        Ok(Some(cfg))
    }

    /// The bus a component that publishes or consumes events must use.
    /// `None` (no `events:` section) is an error, never a bus that quietly
    /// drops everything.
    pub async fn build_required(cfg: Option<&Self>) -> anyhow::Result<Arc<dyn EventBus>> {
        match cfg {
            Some(cfg) => cfg.build().await,
            None => anyhow::bail!(
                "this command publishes or consumes pipeline events but {} has no `events:` \
                 section; set `events.backend` to `nats` (and `events.nats.url`) to run the \
                 event-driven pipeline, or to `noop` to drop publishes deliberately",
                crate::CONFIG_REL_PATH
            ),
        }
    }

    pub async fn build(&self) -> anyhow::Result<Arc<dyn EventBus>> {
        self.validate()
            .map_err(|e| anyhow::anyhow!("invalid `events` section: {e}"))?;
        match self.backend {
            EventsBackend::Noop => Ok(Arc::new(NoOpBus)),
            EventsBackend::Nats => {
                #[cfg(feature = "events-nats")]
                {
                    let connection = self.nats.connection(|name| std::env::var(name).ok())?;
                    let bus = super::nats::NatsBus::connect(connection).await?;
                    Ok(Arc::new(bus))
                }
                #[cfg(not(feature = "events-nats"))]
                {
                    anyhow::bail!("events.backend=nats requires the `events-nats` cargo feature");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<EventBusConfig, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(yaml)
    }

    #[test]
    fn the_backend_must_be_named() {
        for yaml in ["{}", "nats:\n  url: nats://broker:4222\n"] {
            let err = parse(yaml).unwrap_err().to_string();
            assert!(err.contains("backend"), "{yaml}: {err}");
        }
        assert_eq!(parse("backend: noop").unwrap().backend, EventsBackend::Noop);
        assert_eq!(parse("backend: nats").unwrap().backend, EventsBackend::Nats);
        assert!(parse("backend: carrier-pigeon").is_err());
    }

    #[test]
    fn a_misspelt_key_is_an_error_not_an_ignored_setting() {
        let err = parse("backend: nats\nnats:\n  credential_file: /x\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("credential_file"), "{err}");
        assert!(parse("backend: nats\nbackends: nats\n").is_err());
    }

    #[test]
    fn connection_keys_are_checked_against_each_other() {
        for (yaml, needle) in [
            ("credentials_file: /a\ntoken_env: T", "only one of"),
            ("token_env: T\nuser: u\npassword_env: P", "only one of"),
            ("user: u", "go together"),
            ("password_env: P", "go together"),
            ("tls_client_cert: /c", "go together"),
            ("tls_client_key: /k", "go together"),
            ("token_env: \"\"", "must not be empty"),
            ("credentials_file: \"\"", "must not be empty"),
            ("url: \"\"", "must not be empty"),
        ] {
            let nats: NatsYaml = serde_yaml_ng::from_str(yaml).unwrap();
            let err = nats.validate().unwrap_err();
            assert!(err.contains(needle), "{yaml}: {err}");
        }
        let ok: NatsYaml =
            serde_yaml_ng::from_str("user: u\npassword_env: P\ntls_ca_file: /ca").unwrap();
        ok.validate().unwrap();
        NatsYaml::default().validate().unwrap();
    }

    #[test]
    fn an_absent_events_section_is_none_and_a_present_bad_one_names_events() {
        let home = tempfile::tempdir().unwrap();
        let write = |text: &str| {
            let path = home.path().join(crate::CONFIG_REL_PATH);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            ConfigFile::load_in(home.path()).unwrap()
        };
        assert!(EventBusConfig::from_file(&write("storage: {}\n"))
            .unwrap()
            .is_none());
        assert!(EventBusConfig::from_file(&write("events:\n  backend: noop\n"))
            .unwrap()
            .is_some());
        for bad in [
            "events:\n  nats: {url: nats://x}\n",
            "events: nats\n",
            "events:\n  backend: nats\n  nats:\n    user: only-a-user\n",
        ] {
            let err = EventBusConfig::from_file(&write(bad)).unwrap_err().to_string();
            assert!(err.contains("`events`"), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn a_missing_events_section_refuses_to_build_a_bus() {
        let err = EventBusConfig::build_required(None)
            .await
            .err()
            .expect("must refuse")
            .to_string();
        assert!(err.contains("events.backend"), "{err}");
        // Explicit noop is allowed to publish (and drops them on purpose).
        let bus = EventBusConfig::build_required(Some(&EventBusConfig::noop()))
            .await
            .unwrap();
        bus.publish("x.y", b"dropped").await.unwrap();
    }

    #[tokio::test]
    async fn an_invalid_section_is_refused_when_built() {
        let cfg = EventBusConfig {
            backend: EventsBackend::Noop,
            nats: NatsYaml {
                user: Some("u".into()),
                ..NatsYaml::default()
            },
        };
        assert!(cfg.build().await.is_err());
    }
}
