use std::future::Future;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;

mod admin;
mod app;
mod auth;
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
    tracing::info!("Gateway listening on {listen}");

    let result = serve(listener, app).await;

    let _ = logging_handle.shutdown().await;
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
