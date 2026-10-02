// rc.306 P0-8 / P3-11: the distill server binary MUST be built with the
// `cuda` feature. Without it, ort wouldn't link the CUDA provider and we
// would silently run on CPU — directly violating the project's
// "no CPU fallback" non-negotiable. Refuse to compile.
#[cfg(not(feature = "cuda"))]
compile_error!(
    "hs-distill-server requires --features cuda. Rebuild with:\n\
     cargo build --release -p hs-distill --features server,cuda --bin hs-distill-server"
);

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use hs_distill::api::DistillServerState;
use hs_distill::collection::CollectionSpec;
use hs_distill::config::DistillServerConfig;
use hs_distill::embed::onnx::OnnxEmbedder;
use hs_distill::embed::Embedder;
use hs_distill::qdrant::{ensure_collection, QdrantStore};
use hs_distill::server;

#[derive(Parser, Debug)]
#[command(name = "hs-distill-server", about = "Distill embedding server")]
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
    // Must run before ANY dlopen or tokio init — re-execs self with the
    // platform's dynamic-lib search path augmented so ort's CUDA provider
    // (Linux) loads from our bundled directories.
    hs_common::service::lib_bootstrap::ensure_lib_paths_or_reexec();
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
    // Every route but /health and /readiness requires this secret; there is
    // no unauthenticated mode.
    let token = server::backend_token(|name| std::env::var(name))?;

    // Build the embedder on the configured device. There is no fallback:
    // if CUDA is unavailable or the model does not land on the GPU,
    // startup fails.
    let embedder = OnnxEmbedder::new(&config.embedding, config.embedding.compute_device.clone())
        .map_err(|e| anyhow::anyhow!("Failed to initialize embedder: {e}"))?;

    tracing::info!("Embedder device: {}", embedder.device());

    // Connect to Qdrant
    let qdrant = qdrant_client::Qdrant::from_url(&config.qdrant_url)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to connect to Qdrant: {e}"))?;

    // Create or verify every collection the server will serve. Requests can
    // only name these; none is ever created on demand.
    let spec = CollectionSpec::new(embedder.dimension(), &config.hnsw);
    for name in config.served_collections() {
        let outcome = ensure_collection(&qdrant, name, &spec)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to ensure collection '{name}': {e}"))?;
        tracing::info!(
            collection = name,
            created = outcome.created,
            indexes_created = ?outcome.indexes_created,
            hnsw_disabled = outcome.hnsw_disabled,
            "collection ready"
        );
    }

    let host = args.host.unwrap_or_else(|| config.host.clone());
    let port = args.port.unwrap_or(config.port);
    let store = QdrantStore::new(qdrant, config.hnsw.search_ef);
    let state = Arc::new(DistillServerState::new(
        Arc::new(embedder),
        Arc::new(store),
        config,
    ));

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
