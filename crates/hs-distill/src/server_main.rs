use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use hs_common::stores::Stores;
use hs_distill::api::DistillServerState;
use hs_distill::config::DistillServerConfig;
use hs_distill::embed_device::{
    ollama_embed_device, verify_embed_capacity, DEVICE_CUDA, OLLAMA_EMBED_MODEL,
    OLLAMA_EMBED_MODEL_SETUP,
};
use hs_distill::myelin::MyelinStore;
use hs_distill::server;
use hs_distill::store::DocumentStore;

#[derive(Parser, Debug)]
#[command(
    name = "hs-distill-server",
    about = "Distill server: chunks documents into the myelin store and searches them"
)]
struct Args {
    /// Bind address (default: `distill_server.host` from the config, else 0.0.0.0)
    #[arg(long)]
    host: Option<String>,
    /// Listen port (default: `distill_server.port` from the config, else 7434)
    #[arg(long)]
    port: Option<u16>,
}

include!("../../../build-support/version_marker.rs");

fn main() -> Result<()> {
    keep_version_marker();
    // Secrets are exported into the environment, which is only sound while
    // this is the only thread: load them before the runtime (and its worker
    // threads) exist, and refuse to start if they cannot be read. The
    // backend token read below depends on this having run.
    hs_common::secrets::load_default_secrets().context("loading ~/.home-still/secrets.env")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let logging_handle = install_logging().await;
    let args = Args::parse();

    // No fallback: a malformed `distill_server:` section or a bad
    // HS_DISTILL_* value stops the start instead of running on defaults.
    let config = DistillServerConfig::load().context("loading the distill server configuration")?;
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid distill_server config: {e}"))?;
    // The registry of stores: which tenant holds each one, and the personal
    // prefix from the config. A malformed registry stops the start.
    let stores = Stores::load().context("loading the store registry")?;
    // Every route but /health and /readiness requires this secret; there is
    // no unauthenticated mode.
    let token = server::backend_token(|name| std::env::var(name))?;

    // The store and the embedder behind it must both answer before the
    // server listens. There is no fallback: a store that is down, or an
    // embedding model that is not fully on the GPU, stops the start.
    let store = MyelinStore::new(&config.myelin_url)?;
    let backend = store
        .health()
        .await
        .map_err(|e| anyhow::anyhow!("myelin is not usable at {}: {e}", config.myelin_url))?;
    tracing::info!(%backend, url = %config.myelin_url, "myelin reachable");

    let ollama = hs_common::http::client_builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(config.ollama_timeout_secs))
        .build()
        .context("building the ollama HTTP client")?;
    let device = ollama_embed_device(&ollama, &config.ollama_url)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "cannot verify where {OLLAMA_EMBED_MODEL} runs: {e}. An ollama without the \
                 model needs: {OLLAMA_EMBED_MODEL_SETUP}"
            )
        })?;
    if device != DEVICE_CUDA {
        anyhow::bail!(
            "ollama runs {OLLAMA_EMBED_MODEL} on `{device}`, not fully on the GPU. Distill \
             embedding never runs on the CPU: fix ollama's CUDA (driver, libraries, free VRAM)"
        );
    }
    verify_embed_capacity(&ollama, &config.ollama_url)
        .await
        .map_err(|e| anyhow::anyhow!("{OLLAMA_EMBED_MODEL} cannot embed a long chunk: {e}"))?;
    tracing::info!(model = OLLAMA_EMBED_MODEL, %device, "embedding model on the GPU");

    let host = args.host.unwrap_or_else(|| config.host.clone());
    let port = args.port.unwrap_or(config.port);
    let myelin_url = config.myelin_url.clone();
    let state = Arc::new(DistillServerState::new(
        Arc::new(store),
        ollama,
        config,
        stores,
    ));
    // Myelin must hold every store under the tenant the registry names
    // before a request can file a document in one.
    state
        .define_stores()
        .await
        .map_err(|e| anyhow::anyhow!("myelin at {myelin_url} refused the store registry: {e}"))?;
    tracing::info!(
        stores = state.stores.iter().count(),
        "stores defined in myelin"
    );

    let addr = format!("{host}:{port}");
    tracing::info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let result = axum::serve(listener, server::app(state, token)).await;

    let _ = logging_handle.shutdown().await;
    result?;
    Ok(())
}

async fn install_logging() -> hs_common::logging::LoggingHandle {
    use hs_common::logging::{self, StderrOutput};
    const SERVICE: &str = "hs-distill-server";
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
