use std::future::Future;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;

mod admin;
mod app;
mod auth;
mod backend_health;
mod backend_url;
mod balancer;
mod config;
mod enrollment;
mod oauth;
mod proxy;
mod ratelimit;
mod revocation;
mod state;
mod store;
#[cfg(test)]
mod testutil;

use auth::SigningKeys;
use config::GatewayConfig;
use revocation::Revocations;
use state::GatewayState;

/// How long in-flight requests get to finish after SIGTERM/SIGINT before the
/// process exits anyway (a long scribe stream must not block a restart).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// hs-gateway — authenticated reverse proxy for home-still cloud access
#[derive(Parser)]
#[command(name = "hs-gateway", version = env!("HS_VERSION"))]
struct Args {
    /// Override `cloud.gateway.listen` from config.yaml
    #[arg(long)]
    listen: Option<String>,

    /// Public https origin of this gateway, e.g. https://cloud.example.com.
    /// Required: it is published in the OAuth metadata.
    #[arg(long)]
    gateway_url: Option<String>,
}

include!("../../../build-support/version_marker.rs");

fn main() -> anyhow::Result<()> {
    keep_version_marker();
    // Secrets are exported into the environment, which is only sound while
    // this is the only thread: load them before the runtime (and its worker
    // threads) exist, and refuse to start if they cannot be read. The
    // backend token read in `async_main` depends on this having run.
    hs_common::secrets::load_default_secrets().context("loading ~/.home-still/secrets.env")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let logging_handle = install_logging().await;

    let args = Args::parse();
    let config = GatewayConfig::load()?;
    let gateway_url = config::validate_gateway_url(args.gateway_url.as_deref())?;
    let keys = SigningKeys::load(&config)?;
    let admin_key = hs_common::auth::token::load_or_create_admin_key(&config.admin_key_path())?;
    let revocations = Revocations::load(config.revocation_path())?;
    // Required: backends reject requests without it, so there is no
    // "send nothing" mode. Secrets were loaded into the environment above.
    let backend_token = hs_common::auth::backend::BackendToken::from_env()?;

    let listen = args.listen.unwrap_or_else(|| config.listen.clone());

    tracing::info!("Starting gateway on {listen} (public URL {gateway_url})");
    tracing::info!("Routes: {:?}", config.routes.keys().collect::<Vec<_>>());

    let routes = config.routes.clone();

    let state = Arc::new(GatewayState::new(
        config,
        keys,
        admin_key,
        revocations,
        gateway_url,
        backend_token,
    )?);
    let app = app::build_router(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;

    let result = serve_and_report(listener, app, &listen, async move {
        backend_health::report(&routes).await
    })
    .await;

    let _ = logging_handle.shutdown().await;
    result
}

/// Log that the gateway is listening, start `backend_report` in the
/// background, and serve until SIGINT/SIGTERM.
///
/// Are the backends actually reachable? The gateway proxies to other hosts,
/// and a backend it cannot reach makes every request to that service fail —
/// with a `502` or a `504` in a browser and nothing anywhere saying why. The
/// gateway's own host is the only vantage point from which "can a client
/// reach the backend?" is the same question: a backend that answers its own
/// `/health` on its own host proves nothing, because a firewall that drops
/// the port leaves the service perfectly healthy in place and unreachable
/// from everywhere else.
///
/// A backend that is down is **not** fatal: the gateway's job is to proxy,
/// and one host rebooting must not take the other services down with it. So
/// the probes only report, loudly. They also must not delay the gateway: a
/// backend that drops packets costs the probe's whole timeout, and the
/// gateway used to sit unbound for that long (BACKLOG P2-16: `Gateway
/// listening` logged six seconds after the unit started). The socket is
/// already bound, so the probes run in a task beside the server and their
/// results are logged after `Gateway listening`.
async fn serve_and_report<R>(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    listen: &str,
    backend_report: R,
) -> anyhow::Result<()>
where
    R: Future<Output = Vec<String>> + Send + 'static,
{
    tracing::info!("Gateway listening on {listen}");
    let probes = tokio::spawn(async move {
        for line in backend_report.await {
            tracing::warn!("{line}");
        }
    });
    let result = serve(listener, app).await;
    probes.abort();
    result
}

/// Serve until SIGINT/SIGTERM, then drain in-flight requests for at most
/// [`SHUTDOWN_GRACE`].
async fn serve(listener: tokio::net::TcpListener, app: axum::Router) -> anyhow::Result<()> {
    let signal = shutdown_signal()?;
    let (begin_drain, drain) = tokio::sync::oneshot::channel::<()>();

    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = drain.await;
        })
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => result?,
        () = signal => {
            tracing::info!("Shutting down gateway");
            let _ = begin_drain.send(());
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(result) => result?,
                Err(_) => tracing::warn!(
                    "in-flight requests did not finish within {}s; exiting anyway",
                    SHUTDOWN_GRACE.as_secs()
                ),
            }
        }
    }
    Ok(())
}

/// A future that resolves on SIGINT or SIGTERM. Handlers are installed before
/// serving starts, so failing to install them is a startup error.
#[cfg(unix)]
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

async fn install_logging() -> hs_common::logging::LoggingHandle {
    use hs_common::logging::{self, StderrOutput};
    const SERVICE: &str = "hs-gateway";
    let sections = logging::load_config_sections()
        .unwrap_or_else(|e| logging::exit_on_config_error(SERVICE, e));
    let cfg = sections
        .logging_config(SERVICE, StderrOutput::EnvFilter("info".into()))
        .unwrap_or_else(|e| logging::exit_on_config_error(SERVICE, e));
    let mut handle = logging::init(cfg);
    handle
        .start_shipping(sections.storage.as_ref(), &sections.logs.bucket)
        .await;
    handle
}

#[cfg(test)]
mod tests {
    use hs_common::auth::backend::{BackendToken, ENV_VAR};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One `GET /ping` over a raw connection; the status line of the answer.
    async fn ping(addr: std::net::SocketAddr) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: gateway\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer.lines().next().unwrap_or_default().to_string()
    }

    /// A backend probe that takes as long as a dropped packet does must not
    /// keep the gateway from answering: the report is still pending (its
    /// gate is never opened) while a request is served.
    #[tokio::test]
    async fn the_gateway_serves_while_its_backend_check_is_still_running() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let (never_opened, gate) = tokio::sync::oneshot::channel::<()>();
        let (probe_started, started) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            super::serve_and_report(listener, app, "test", async move {
                let _ = probe_started.send(());
                let _ = gate.await;
                vec!["distill backend is not reachable".to_string()]
            })
            .await
        });

        started.await.expect("the backend check starts");
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), ping(addr))
            .await
            .expect("the gateway answers while the check is pending");
        assert!(status.contains("200"), "{status}");
        assert!(!server.is_finished());

        drop(never_opened);
        server.abort();
    }

    fn lookup(value: Option<&'static str>) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |name| {
            assert_eq!(name, ENV_VAR);
            value
                .map(str::to_string)
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    /// The gateway starts with `BackendToken::from_env()`; an unset or short
    /// value must be an error naming the variable, never a "send nothing" mode.
    #[test]
    fn startup_refuses_without_a_usable_backend_token() {
        for value in [
            None,
            Some("short"),
            Some("has a space in it 0123456789abcdefghij"),
        ] {
            let err = BackendToken::from_lookup(lookup(value)).expect_err("must refuse");
            let msg = format!("{err:#}");
            assert!(msg.contains(ENV_VAR), "{msg}");
            if let Some(v) = value {
                assert!(!msg.contains(v), "secret leaked into the error: {msg}");
            }
        }
        assert!(
            BackendToken::from_lookup(lookup(Some("0123456789abcdef0123456789abcdef"))).is_ok()
        );
    }
}
