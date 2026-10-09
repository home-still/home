use anyhow::Context;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use std::path::PathBuf;
use std::time::Duration;

use super::config::{redact_url, NatsYaml};
use super::{ConsumerSpec, Event, EventBus, EventStream, QueueDepth};

/// How the client authenticates to the broker. At most one method.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum NatsAuth {
    /// No credentials (a broker that needs none, e.g. on loopback).
    #[default]
    None,
    /// A NATS `.creds` file (JWT + NKey seed).
    CredentialsFile(PathBuf),
    Token(String),
    UserPassword {
        user: String,
        password: String,
    },
}

impl std::fmt::Debug for NatsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::CredentialsFile(path) => f.debug_tuple("CredentialsFile").field(path).finish(),
            Self::Token(_) => f.write_str("Token(<redacted>)"),
            Self::UserPassword { user, .. } => f
                .debug_struct("UserPassword")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// TLS settings beyond what the URL scheme (`tls://`) implies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NatsTls {
    /// PEM bundle of the CA that signed the broker's certificate, for a
    /// private CA the system store does not know.
    pub ca_file: Option<PathBuf>,
    /// Client certificate and key (PEM) for mutual TLS.
    pub client_cert_and_key: Option<(PathBuf, PathBuf)>,
    /// Refuse to talk to a broker that does not offer TLS.
    pub require: bool,
}

#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
    /// Per-message processing deadline. Used as the JetStream consumer's
    /// `ack_wait` — if the broker gets neither an ack nor an
    /// [`Event::in_progress`](super::Event::in_progress) heartbeat within
    /// this window, it treats the delivery as lost and redelivers. The
    /// watchers heartbeat every `ack_wait / 3`, so it need not cover a
    /// whole handler run.
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
    pub auth: NatsAuth,
    pub tls: NatsTls,
}

impl Default for NatsConfig {
    fn default() -> Self {
        // Mirror NatsYaml::default() in config.rs — the YAML path is what
        // every live consumer goes through, and two divergent defaults
        // for the same constant would leave consumers on different
        // redelivery windows depending on which code path built the config.
        Self {
            url: "nats://localhost:4222".into(),
            ack_wait: Duration::from_secs(7200),
            max_deliver: 5,
            max_age: Duration::from_secs(7 * 24 * 3600),
            max_ack_pending: 32,
            auth: NatsAuth::None,
            tls: NatsTls::default(),
        }
    }
}

impl NatsYaml {
    /// The connection settings the YAML describes. `env` looks up an
    /// environment variable (the process environment in production): the
    /// token and the password never appear in the config file, only the
    /// name of the variable that holds them (`secrets.env` exports it).
    ///
    /// An auth variable that is unset or empty is an error naming the
    /// variable, never a connection without credentials.
    pub fn connection(&self, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<NatsConfig> {
        self.validate()
            .map_err(|e| anyhow::anyhow!("events.nats: {e}"))?;
        let secret = |key: &str, var: &str| -> anyhow::Result<String> {
            match env(var) {
                Some(value) if !value.is_empty() => Ok(value),
                _ => anyhow::bail!(
                    "events.nats.{key} names the environment variable `{var}`, which is not set \
                     (or is empty); export it or add it to ~/.home-still/secrets.env"
                ),
            }
        };
        let auth = if let Some(path) = &self.credentials_file {
            NatsAuth::CredentialsFile(path.clone())
        } else if let Some(var) = &self.token_env {
            NatsAuth::Token(secret("token_env", var)?)
        } else if let (Some(user), Some(var)) = (&self.user, &self.password_env) {
            NatsAuth::UserPassword {
                user: user.clone(),
                password: secret("password_env", var)?,
            }
        } else {
            NatsAuth::None
        };
        Ok(NatsConfig {
            url: self.url.clone(),
            ack_wait: Duration::from_secs(self.ack_wait_secs),
            max_deliver: self.max_deliver,
            max_age: Duration::from_secs(self.max_age_secs),
            max_ack_pending: self.max_ack_pending,
            auth,
            tls: NatsTls {
                ca_file: self.tls_ca_file.clone(),
                client_cert_and_key: self
                    .tls_client_cert
                    .clone()
                    .zip(self.tls_client_key.clone()),
                require: self.require_tls,
            },
        })
    }
}

/// JetStream names used by the pipeline. Paired with subjects so the
/// stream captures everything that might land on a matching subject
/// without the publisher needing to know the stream name.
const PAPERS_STREAM: &str = "PAPERS";
const PAPERS_SUBJECTS: &[&str] = &["papers.>"];
const SCRIBE_STREAM: &str = "SCRIBE";
const SCRIBE_SUBJECTS: &[&str] = &["scribe.>"];
const DISTILL_STREAM: &str = "DISTILL";
const DISTILL_SUBJECTS: &[&str] = &["distill.>"];

pub struct NatsBus {
    jetstream: async_nats::jetstream::Context,
    cfg: NatsConfig,
}

/// A file named by the config must exist before a connection attempt, so the
/// failure names the key's file instead of surfacing as a bare TLS error.
async fn require_file(what: &str, path: &std::path::Path) -> anyhow::Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => anyhow::bail!("{what} {} is not a file", path.display()),
        Err(e) => anyhow::bail!("{what} {}: {e}", path.display()),
    }
}

/// The `async-nats` connect options for `cfg`: credentials and TLS, nothing
/// else. Fails (naming the file) when a configured file cannot be used.
pub async fn connect_options(cfg: &NatsConfig) -> anyhow::Result<async_nats::ConnectOptions> {
    use async_nats::ConnectOptions;

    let mut options = match &cfg.auth {
        NatsAuth::None => ConnectOptions::new(),
        NatsAuth::CredentialsFile(path) => {
            require_file("events.nats.credentials_file", path).await?;
            ConnectOptions::with_credentials_file(path)
                .await
                .with_context(|| {
                    format!(
                        "events.nats.credentials_file {} is not a usable NATS credentials file",
                        path.display()
                    )
                })?
        }
        NatsAuth::Token(token) => ConnectOptions::with_token(token.clone()),
        NatsAuth::UserPassword { user, password } => {
            ConnectOptions::with_user_and_password(user.clone(), password.clone())
        }
    };
    if let Some(ca) = &cfg.tls.ca_file {
        require_file("events.nats.tls_ca_file", ca).await?;
        options = options.add_root_certificates(ca.clone());
    }
    if let Some((cert, key)) = &cfg.tls.client_cert_and_key {
        require_file("events.nats.tls_client_cert", cert).await?;
        require_file("events.nats.tls_client_key", key).await?;
        options = options.add_client_certificate(cert.clone(), key.clone());
    }
    if cfg.tls.require {
        options = options.require_tls(true);
    }
    Ok(options)
}

impl NatsBus {
    pub async fn connect(cfg: NatsConfig) -> anyhow::Result<Self> {
        let bus = Self::connect_unprovisioned(cfg).await?;
        // Provision both streams up-front so the first publish / consume
        // on a cold broker doesn't race. get_or_create is idempotent.
        bus.provision_streams().await?;
        Ok(bus)
    }

    /// Connect without touching the streams. Only [`Self::reset_streams`]
    /// uses this: the repair for a stream whose live config cannot be
    /// updated in place must be able to connect while [`Self::connect`]
    /// refuses to.
    pub async fn connect_unprovisioned(cfg: NatsConfig) -> anyhow::Result<Self> {
        let client = connect_options(&cfg)
            .await?
            .connect(&cfg.url)
            .await
            .with_context(|| format!("connecting to NATS at {}", redact_url(&cfg.url)))?;
        let jetstream = async_nats::jetstream::new(client);
        Ok(Self { jetstream, cfg })
    }

    async fn provision_streams(&self) -> anyhow::Result<()> {
        self.ensure_stream(PAPERS_STREAM, PAPERS_SUBJECTS).await?;
        self.ensure_stream(SCRIBE_STREAM, SCRIBE_SUBJECTS).await?;
        self.ensure_stream(DISTILL_STREAM, DISTILL_SUBJECTS).await
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
        // `get_or_create_stream` keeps an existing stream's config whatever
        // ours says, so — like the consumers — the live config is compared
        // with the wanted one and brought in line with `update_stream`.
        // Retention and storage cannot change on a live stream: that is an
        // error naming the field, never a delete (the stream holds messages).
        let stream = self
            .jetstream
            .get_or_create_stream(config.clone())
            .await
            .map_err(|e| anyhow::anyhow!("create stream {name}: {e}"))?;
        let found = StreamShape::of(&stream.cached_info().config);
        let drift = found.drift_from(&StreamShape::of(&config));
        if drift.is_empty() {
            return Ok(());
        }
        let fixed: Vec<&Drift> = drift.iter().filter(|d| !d.mutable).collect();
        if !fixed.is_empty() {
            return Err(immutable_stream_drift_error(name, &fixed));
        }
        let names = drift.iter().map(|d| d.field).collect::<Vec<_>>().join(", ");
        tracing::warn!(
            stream = name,
            differs = %names,
            "existing stream does not match the configured one; updating it in place"
        );
        self.jetstream
            .update_stream(config)
            .await
            .map_err(|e| anyhow::anyhow!("update stream {name} (differs in {names}): {e}"))?;
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
        // The durable consumer outlives the watcher. A restart finds it and
        // keeps using it (deleting it on every start made two watchers that
        // share a durable name tear each other's consumer down, each exit
        // and restart in turn, and reset every message's delivery count, so
        // a poison message was never dead-lettered). Two processes on one
        // durable now load-balance, as the spec says they should.
        //
        // `get_or_create_consumer` keeps an existing consumer's config
        // whatever ours says (a stale test consumer once pinned production
        // to ack_wait=10s), so the existing config is compared with the
        // wanted one and brought in line with `update_consumer` — in place,
        // without deleting the consumer or its pending messages.
        let consumer = stream
            .get_or_create_consumer(spec.durable_name, config.clone())
            .await
            .map_err(|e| anyhow::anyhow!("get or create consumer {}: {e}", spec.durable_name))?;
        let found = ConsumerShape::of_found(&consumer.cached_info().config);
        let drift = found.drift_from(&ConsumerShape::of_wanted(&config));
        if drift.is_empty() {
            return Ok(consumer);
        }
        let fixed: Vec<&Drift> = drift.iter().filter(|d| !d.mutable).collect();
        if !fixed.is_empty() {
            return Err(immutable_drift_error(
                spec.durable_name,
                spec.stream,
                &fixed,
            ));
        }
        let names = drift.iter().map(|d| d.field).collect::<Vec<_>>().join(", ");
        tracing::warn!(
            consumer = spec.durable_name,
            differs = %names,
            "existing consumer does not match the configured one; updating it in place"
        );
        stream.update_consumer(config).await.map_err(|e| {
            anyhow::anyhow!(
                "update consumer {} (differs in {names}): {e}",
                spec.durable_name
            )
        })
    }

    /// Delete every pipeline stream (PAPERS, SCRIBE, DISTILL) and recreate
    /// it from the current [`NatsConfig`]. All queued and in-flight
    /// messages are discarded. Operators use this to recover from config
    /// drift (e.g. a stream whose retention differs, which cannot be
    /// updated in place). Meant for a bus from
    /// [`Self::connect_unprovisioned`]. The list must match what
    /// [`Self::connect`] provisions: resetting a subset leaves the
    /// omitted stream carrying exactly the drifted config the operator
    /// is trying to clear.
    pub async fn reset_streams(&self) -> anyhow::Result<()> {
        for stream in [PAPERS_STREAM, SCRIBE_STREAM, DISTILL_STREAM] {
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
        self.provision_streams().await
    }
}
/// The settings of a pull consumer that this crate sets and that can drift
/// from the configuration (the other settings are left at the server's
/// defaults on both sides).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsumerShape {
    ack_wait: Duration,
    max_deliver: i64,
    max_ack_pending: i64,
    filter_subject: String,
    ack_policy: String,
    deliver_policy: String,
    backoff: Vec<Duration>,
}

/// One setting in which the live consumer differs from the wanted one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drift {
    field: &'static str,
    live: String,
    wanted: String,
    /// JetStream accepts a change of this setting on an existing consumer.
    /// `ack_policy` and `deliver_policy` are fixed at creation.
    mutable: bool,
}

impl ConsumerShape {
    fn of_found(config: &async_nats::jetstream::consumer::Config) -> Self {
        Self {
            ack_wait: config.ack_wait,
            max_deliver: config.max_deliver,
            max_ack_pending: config.max_ack_pending,
            filter_subject: config.filter_subject.clone(),
            ack_policy: format!("{:?}", config.ack_policy),
            deliver_policy: format!("{:?}", config.deliver_policy),
            backoff: config.backoff.clone(),
        }
    }

    fn of_wanted(config: &async_nats::jetstream::consumer::pull::Config) -> Self {
        Self {
            ack_wait: config.ack_wait,
            max_deliver: config.max_deliver,
            max_ack_pending: config.max_ack_pending,
            filter_subject: config.filter_subject.clone(),
            ack_policy: format!("{:?}", config.ack_policy),
            deliver_policy: format!("{:?}", config.deliver_policy),
            backoff: config.backoff.clone(),
        }
    }

    /// The settings in which `self` (what the server has) differs from
    /// `wanted`. Empty: the consumer can be used as it is.
    fn drift_from(&self, wanted: &Self) -> Vec<Drift> {
        let mut drift = Vec::new();
        let mut check = |field, live: String, want: String, mutable, same: bool| {
            if !same {
                drift.push(Drift {
                    field,
                    live,
                    wanted: want,
                    mutable,
                });
            }
        };
        check(
            "ack_wait",
            format!("{:?}", self.ack_wait),
            format!("{:?}", wanted.ack_wait),
            true,
            self.ack_wait == wanted.ack_wait,
        );
        check(
            "max_deliver",
            self.max_deliver.to_string(),
            wanted.max_deliver.to_string(),
            true,
            self.max_deliver == wanted.max_deliver,
        );
        check(
            "max_ack_pending",
            self.max_ack_pending.to_string(),
            wanted.max_ack_pending.to_string(),
            true,
            self.max_ack_pending == wanted.max_ack_pending,
        );
        check(
            "filter_subject",
            self.filter_subject.clone(),
            wanted.filter_subject.clone(),
            true,
            self.filter_subject == wanted.filter_subject,
        );
        check(
            "backoff",
            format!("{:?}", self.backoff),
            format!("{:?}", wanted.backoff),
            true,
            self.backoff == wanted.backoff,
        );
        check(
            "ack_policy",
            self.ack_policy.clone(),
            wanted.ack_policy.clone(),
            false,
            self.ack_policy == wanted.ack_policy,
        );
        check(
            "deliver_policy",
            self.deliver_policy.clone(),
            wanted.deliver_policy.clone(),
            false,
            self.deliver_policy == wanted.deliver_policy,
        );
        drift
    }
}

/// The error for drift JetStream cannot repair in place. The watcher never
/// deletes a consumer itself: it may hold messages another process is
/// working on.
fn immutable_drift_error(durable: &str, stream: &str, drift: &[&Drift]) -> anyhow::Error {
    let fields = drift
        .iter()
        .map(|d| format!("{} (live: {}, wanted: {})", d.field, d.live, d.wanted))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::anyhow!(
        "consumer {durable} on stream {stream} differs in settings JetStream cannot change on an \
         existing consumer: {fields}. Either restore the configuration to the live values, or \
         delete the consumer once it has no pending or ack-pending messages \
         (`nats consumer info {stream} {durable}`, then `nats consumer rm {stream} {durable}`) \
         and start the watcher again; it recreates the consumer from the configuration"
    )
}

/// The settings of a stream that this crate sets and that can drift from the
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamShape {
    /// Sorted: the server may list subjects in any order.
    subjects: Vec<String>,
    retention: String,
    storage: String,
    discard: String,
    max_age: Duration,
}

impl StreamShape {
    fn of(config: &async_nats::jetstream::stream::Config) -> Self {
        let mut subjects = config.subjects.clone();
        subjects.sort();
        Self {
            subjects,
            retention: format!("{:?}", config.retention),
            storage: format!("{:?}", config.storage),
            discard: format!("{:?}", config.discard),
            max_age: config.max_age,
        }
    }

    /// The settings in which `self` (what the server has) differs from
    /// `wanted`. `retention` and `storage` are fixed at creation.
    fn drift_from(&self, wanted: &Self) -> Vec<Drift> {
        let mut drift = Vec::new();
        let mut check = |field, live: String, want: String, mutable| {
            if live != want {
                drift.push(Drift {
                    field,
                    live,
                    wanted: want,
                    mutable,
                });
            }
        };
        check(
            "subjects",
            format!("{:?}", self.subjects),
            format!("{:?}", wanted.subjects),
            true,
        );
        check(
            "max_age",
            format!("{:?}", self.max_age),
            format!("{:?}", wanted.max_age),
            true,
        );
        check(
            "discard",
            self.discard.clone(),
            wanted.discard.clone(),
            true,
        );
        check(
            "retention",
            self.retention.clone(),
            wanted.retention.clone(),
            false,
        );
        check(
            "storage",
            self.storage.clone(),
            wanted.storage.clone(),
            false,
        );
        drift
    }
}

/// The error for stream drift JetStream cannot repair in place. Nothing
/// deletes a stream on its own: it holds queued messages.
fn immutable_stream_drift_error(stream: &str, drift: &[&Drift]) -> anyhow::Error {
    let fields = drift
        .iter()
        .map(|d| format!("{} (live: {}, wanted: {})", d.field, d.live, d.wanted))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::anyhow!(
        "stream {stream} differs in settings JetStream cannot change on an existing stream: \
         {fields}. Either restore the configuration to the live values, or discard the \
         stream's queued messages and recreate it from the configuration with \
         `hs pipeline events-reset`"
    )
}

/// Turn the broker's message stream into an [`EventStream`] in which a
/// delivery error is an `Err` item, and the last one: the stream ends right
/// after it.
///
/// `async-nats` reports a deleted consumer, a missed idle heartbeat (the
/// connection dropped and the pull request was lost) or a server error as
/// `Err` items. Dropping them left a subscriber waiting on a stream that
/// would never deliver again (a missed heartbeat does not end the stream),
/// reporting nothing. The subscriber must see the error, stop, and exit
/// non-zero so its supervisor starts a fresh consumer.
fn surface_errors<S, M, E>(
    messages: S,
    to_event: impl Fn(M) -> Event + Send + 'static,
) -> EventStream
where
    S: Stream<Item = Result<M, E>> + Send + 'static,
    M: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let state = (Box::pin(messages), to_event, false);
    let stream = futures::stream::unfold(state, |(mut messages, to_event, failed)| async move {
        // After an error the stream is over: do not wait on the broker for
        // another item (a stream that stopped delivering would never
        // produce one).
        if failed {
            return None;
        }
        let item = messages.next().await?;
        Some(match item {
            Ok(message) => (Ok(to_event(message)), (messages, to_event, false)),
            Err(e) => (
                Err(anyhow::anyhow!("jetstream delivery error: {e}")),
                (messages, to_event, true),
            ),
        })
    })
    .fuse();
    Box::pin(stream)
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
        Ok(surface_errors(messages, Event::from_jetstream))
    }

    async fn queue_depth(&self, spec: &ConsumerSpec) -> anyhow::Result<QueueDepth> {
        let stream = self
            .jetstream
            .get_stream(spec.stream)
            .await
            .map_err(|e| anyhow::anyhow!("get stream {}: {e}", spec.stream))?;
        // Read the broker's live counters; `consumer_info` is a fresh request,
        // unlike the info cached when the consumer handle was built. A missing
        // consumer (no watcher has ever subscribed) is an error, not depth 0.
        let info = stream.consumer_info(spec.durable_name).await.map_err(|e| {
            anyhow::anyhow!("consumer {} on {}: {e}", spec.durable_name, spec.stream)
        })?;
        Ok(QueueDepth {
            pending: info.num_pending,
            ack_pending: info.num_ack_pending as u64,
        })
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
            ..NatsConfig::default()
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
            .expect("stream closed")
            .expect("delivery error");
        assert_eq!(got.subject, "papers.ingested");
        assert_eq!(got.payload, b"hello-jetstream");
        got.ack().await.unwrap();
    }

    fn event_of((subject, payload): (&'static str, &'static str)) -> Event {
        Event::inert(subject, payload)
    }

    #[tokio::test]
    async fn a_delivery_error_is_an_item_and_the_last_one() {
        // A consumer deleted mid-stream, then (hypothetically) more output:
        // everything before the error is delivered, the error carries the
        // cause, and nothing follows it.
        let broker: Vec<Result<(&str, &str), &str>> = vec![
            Ok(("papers.ingested", "one")),
            Ok(("papers.ingested", "two")),
            Err("consumer deleted"),
            Ok(("papers.ingested", "after the error")),
        ];
        let mut stream = surface_errors(futures::stream::iter(broker), event_of);

        assert_eq!(stream.next().await.unwrap().unwrap().payload, b"one");
        assert_eq!(stream.next().await.unwrap().unwrap().payload, b"two");
        let err = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(err.contains("consumer deleted"), "{err}");
        assert!(
            stream.next().await.is_none(),
            "the error must end the stream"
        );
        assert!(stream.next().await.is_none(), "and stay ended");
    }

    #[tokio::test]
    async fn a_missed_heartbeat_that_would_not_end_the_stream_still_surfaces() {
        // async-nats keeps yielding after a missed idle heartbeat; the
        // adaptor must not wait for an end that never comes.
        let broker = futures::stream::iter(vec![Err::<(&str, &str), _>("missed idle heartbeat")])
            .chain(futures::stream::pending());
        let mut stream = surface_errors(broker, event_of);
        let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("the error must arrive without waiting for the broker");
        assert!(first.unwrap().is_err());
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("ended, not pending")
            .is_none());
    }

    #[tokio::test]
    async fn a_clean_stream_delivers_everything_and_ends_cleanly() {
        let broker: Vec<Result<(&str, &str), &str>> = vec![Ok(("a.b", "x")), Ok(("a.b", "y"))];
        let mut stream = surface_errors(futures::stream::iter(broker), event_of);
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.is_none());
    }

    fn yaml(extra: &str) -> NatsYaml {
        serde_yaml_ng::from_str(&format!(
            "url: nats://broker.example.internal:4222\n{extra}"
        ))
        .unwrap()
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn connection_without_auth_keys_is_an_anonymous_plain_connection() {
        let cfg = yaml("").connection(env_of(&[])).unwrap();
        assert_eq!(cfg.auth, NatsAuth::None);
        assert_eq!(cfg.tls, NatsTls::default());
        assert_eq!(cfg.url, "nats://broker.example.internal:4222");
    }

    #[test]
    fn each_auth_method_maps_and_secrets_come_from_the_environment() {
        let cfg = yaml("credentials_file: /etc/hs/nats.creds")
            .connection(env_of(&[]))
            .unwrap();
        assert_eq!(
            cfg.auth,
            NatsAuth::CredentialsFile("/etc/hs/nats.creds".into())
        );

        let cfg = yaml("token_env: HS_NATS_TOKEN")
            .connection(env_of(&[("HS_NATS_TOKEN", "s3cret-token")]))
            .unwrap();
        assert_eq!(cfg.auth, NatsAuth::Token("s3cret-token".into()));

        let cfg = yaml("user: hs\npassword_env: HS_NATS_PASSWORD")
            .connection(env_of(&[("HS_NATS_PASSWORD", "pw")]))
            .unwrap();
        assert_eq!(
            cfg.auth,
            NatsAuth::UserPassword {
                user: "hs".into(),
                password: "pw".into()
            }
        );
    }

    #[test]
    fn an_auth_variable_that_is_unset_or_empty_is_an_error_not_an_anonymous_connection() {
        for env in [env_of(&[]), env_of(&[("HS_NATS_TOKEN", "")])] {
            let err = yaml("token_env: HS_NATS_TOKEN")
                .connection(env)
                .unwrap_err()
                .to_string();
            assert!(err.contains("HS_NATS_TOKEN"), "{err}");
        }
    }

    #[test]
    fn credentials_are_never_printed() {
        let cfg = yaml("user: hs\npassword_env: P")
            .connection(env_of(&[("P", "hunter2-password")]))
            .unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("hunter2-password"), "{shown}");
        let cfg = yaml("token_env: T")
            .connection(env_of(&[("T", "tok-abc123")]))
            .unwrap();
        assert!(!format!("{cfg:?}").contains("tok-abc123"));
    }

    #[test]
    fn tls_keys_map_and_pair_up() {
        let cfg = yaml(
            "tls_ca_file: /etc/ssl/hs-ca.pem\ntls_client_cert: /etc/ssl/c.pem\n\
             tls_client_key: /etc/ssl/k.pem\nrequire_tls: true",
        )
        .connection(env_of(&[]))
        .unwrap();
        assert_eq!(cfg.tls.ca_file, Some("/etc/ssl/hs-ca.pem".into()));
        assert_eq!(
            cfg.tls.client_cert_and_key,
            Some(("/etc/ssl/c.pem".into(), "/etc/ssl/k.pem".into()))
        );
        assert!(cfg.tls.require);
    }

    #[tokio::test]
    async fn a_configured_file_that_is_missing_names_itself() {
        for (auth, tls, needle) in [
            (
                NatsAuth::CredentialsFile("/nonexistent/hs.creds".into()),
                NatsTls::default(),
                "credentials_file",
            ),
            (
                NatsAuth::None,
                NatsTls {
                    ca_file: Some("/nonexistent/ca.pem".into()),
                    ..NatsTls::default()
                },
                "tls_ca_file",
            ),
            (
                NatsAuth::None,
                NatsTls {
                    client_cert_and_key: Some((
                        "/nonexistent/c.pem".into(),
                        "/nonexistent/k.pem".into(),
                    )),
                    ..NatsTls::default()
                },
                "tls_client_cert",
            ),
        ] {
            let cfg = NatsConfig {
                auth,
                tls,
                ..NatsConfig::default()
            };
            let Err(err) = connect_options(&cfg).await else {
                panic!("a missing configured file must fail");
            };
            let err = err.to_string();
            assert!(
                err.contains(needle) && err.contains("/nonexistent/"),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn token_and_password_auth_and_plain_tls_flags_build_options_offline() {
        for auth in [
            NatsAuth::None,
            NatsAuth::Token("t".into()),
            NatsAuth::UserPassword {
                user: "u".into(),
                password: "p".into(),
            },
        ] {
            let cfg = NatsConfig {
                auth,
                tls: NatsTls {
                    require: true,
                    ..NatsTls::default()
                },
                ..NatsConfig::default()
            };
            connect_options(&cfg).await.unwrap();
        }
    }

    fn shape() -> ConsumerShape {
        ConsumerShape {
            ack_wait: Duration::from_secs(7200),
            max_deliver: 5,
            max_ack_pending: 32,
            filter_subject: "papers.ingested".into(),
            ack_policy: "Explicit".into(),
            deliver_policy: "All".into(),
            backoff: Vec::new(),
        }
    }

    #[test]
    fn a_consumer_that_matches_is_reused_untouched() {
        assert!(shape().drift_from(&shape()).is_empty());
    }

    #[test]
    fn every_drifted_setting_is_named_with_live_and_wanted_values() {
        let mut found = shape();
        found.ack_wait = Duration::from_secs(10);
        found.max_deliver = -1;
        found.max_ack_pending = 1000;
        found.filter_subject = "papers.>".into();
        found.backoff = vec![Duration::from_secs(1)];
        found.ack_policy = "None".into();
        found.deliver_policy = "New".into();
        let drift = found.drift_from(&shape());
        let names: Vec<_> = drift.iter().map(|d| d.field).collect();
        assert_eq!(
            names,
            [
                "ack_wait",
                "max_deliver",
                "max_ack_pending",
                "filter_subject",
                "backoff",
                "ack_policy",
                "deliver_policy"
            ]
        );
        let ack = drift.iter().find(|d| d.field == "ack_policy").unwrap();
        assert_eq!(
            (ack.live.as_str(), ack.wanted.as_str()),
            ("None", "Explicit")
        );
    }

    #[test]
    fn only_ack_and_deliver_policy_are_immutable() {
        let mut found = shape();
        found.ack_wait = Duration::from_secs(1);
        found.backoff = vec![Duration::from_secs(1)];
        assert!(found.drift_from(&shape()).iter().all(|d| d.mutable));
        for (field, set) in [("ack_policy", 0), ("deliver_policy", 1)] {
            let mut f = shape();
            if set == 0 {
                f.ack_policy = "None".into()
            } else {
                f.deliver_policy = "New".into()
            }
            let drift = f.drift_from(&shape());
            assert_eq!(drift.len(), 1);
            assert_eq!(drift[0].field, field);
            assert!(!drift[0].mutable);
            let err = immutable_drift_error("d", "S", &[&drift[0]]).to_string();
            assert!(
                err.contains(field) && err.contains("live:") && err.contains("wanted:"),
                "{err}"
            );
            assert!(err.contains("nats consumer rm S d"), "{err}");
        }
    }

    fn stream_shape() -> StreamShape {
        StreamShape {
            subjects: vec!["papers.ingested".into()],
            retention: "WorkQueue".into(),
            storage: "File".into(),
            discard: "Old".into(),
            max_age: Duration::from_secs(7 * 24 * 3600),
        }
    }

    #[test]
    fn a_stream_that_matches_is_left_alone_and_subject_order_is_irrelevant() {
        assert!(stream_shape().drift_from(&stream_shape()).is_empty());
        let cfg = |subjects: &[&str]| async_nats::jetstream::stream::Config {
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(
            StreamShape::of(&cfg(&["b", "a"])),
            StreamShape::of(&cfg(&["a", "b"]))
        );
    }

    #[test]
    fn stream_drift_names_each_field_and_only_retention_and_storage_are_immutable() {
        let mut found = stream_shape();
        found.subjects = vec!["x".into()];
        found.max_age = Duration::from_secs(1);
        found.discard = "New".into();
        let drift = found.drift_from(&stream_shape());
        let names: Vec<_> = drift.iter().map(|d| d.field).collect();
        assert_eq!(names, ["subjects", "max_age", "discard"]);
        assert!(drift.iter().all(|d| d.mutable));

        let mut found = stream_shape();
        found.retention = "Limits".into();
        found.storage = "Memory".into();
        let drift = found.drift_from(&stream_shape());
        assert_eq!(
            drift.iter().map(|d| d.field).collect::<Vec<_>>(),
            ["retention", "storage"]
        );
        assert!(drift.iter().all(|d| !d.mutable));
        let refs: Vec<&Drift> = drift.iter().collect();
        let err = immutable_stream_drift_error("S", &refs).to_string();
        assert!(
            err.contains("retention (live: Limits, wanted: WorkQueue)")
                && err.contains("storage (live: Memory, wanted: File)"),
            "{err}"
        );
    }

    #[test]
    fn the_stock_consumer_configs_convert_to_the_same_shape() {
        use async_nats::jetstream::consumer::{pull::Config as PullConfig, AckPolicy};
        let wanted = PullConfig {
            durable_name: Some("d".into()),
            filter_subject: "scribe.completed".into(),
            ack_policy: AckPolicy::Explicit,
            ack_wait: Duration::from_secs(90),
            max_deliver: 3,
            max_ack_pending: 8,
            ..Default::default()
        };
        // What the server echoes back is the same settings in the generic
        // consumer config.
        let echoed = async_nats::jetstream::consumer::Config {
            durable_name: wanted.durable_name.clone(),
            filter_subject: wanted.filter_subject.clone(),
            ack_policy: wanted.ack_policy,
            ack_wait: wanted.ack_wait,
            max_deliver: wanted.max_deliver,
            max_ack_pending: wanted.max_ack_pending,
            ..Default::default()
        };
        assert_eq!(
            ConsumerShape::of_found(&echoed),
            ConsumerShape::of_wanted(&wanted)
        );
    }
}
