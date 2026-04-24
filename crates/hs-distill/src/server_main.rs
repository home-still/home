// rc.306 P0-8 / P3-11: the distill server binary MUST be built with the
// `cuda` feature. Without it, ort wouldn't link the CUDA provider and we
// would silently run on CPU — directly violating the project's
// "no CPU fallback" non-negotiable. Refuse to compile.
#[cfg(not(feature = "cuda"))]
compile_error!(
    "hs-distill-server requires --features cuda. Rebuild with:\n\
     cargo build --release -p hs-distill --features server,cuda --bin hs-distill-server"
);

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use hs_distill::config::DistillServerConfig;
use hs_distill::embed::{Embedder, FallbackEmbedder};
use hs_distill::server::{self, DistillServerState};

#[derive(Parser, Debug)]
#[command(name = "hs-distill-server", about = "Distill embedding server")]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    #[arg(long, default_value = "7434")]
    port: u16,
}

fn main() -> Result<()> {
    // Must run before ANY dlopen or tokio init — re-execs self with the
    // platform's dynamic-lib search path augmented so ort's CUDA provider
    // (Linux) loads from our bundled directories.
    hs_common::service::lib_bootstrap::ensure_lib_paths_or_reexec();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let _ = hs_common::secrets::load_default_secrets();
    let logging_handle = install_logging().await;
    let args = Args::parse();

    let config = DistillServerConfig::load().unwrap_or_else(|e| {
        tracing::warn!("Config load error: {e}, using defaults");
        DistillServerConfig::default()
    });

    // Build embedder with GPU→CPU fallback
    let embedder = FallbackEmbedder::build(&config.embedding)
        .map_err(|e| anyhow::anyhow!("Failed to initialize embedder: {e}"))?;

    tracing::info!("Embedder device: {}", embedder.device());

    // Connect to Qdrant
    let qdrant = qdrant_client::Qdrant::from_url(&config.qdrant_url)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to connect to Qdrant: {e}"))?;

    // Ensure collection exists
    hs_distill::qdrant::ensure_collection(
        &qdrant,
        &config.collection_name,
        config.embedding.dimension,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to ensure collection: {e}"))?;

    let state = Arc::new(DistillServerState {
        embedder: Arc::new(embedder),
        qdrant: Arc::new(qdrant),
        config,
        in_flight: Arc::new(AtomicUsize::new(0)),
    });

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let result = axum::serve(listener, server::app(state)).await;

    if let Some(h) = logging_handle {
        let _ = h.shutdown().await;
    }
    result?;
    Ok(())
}

async fn install_logging() -> Option<hs_common::logging::LoggingHandle> {
    use hs_common::logging::{self, LoggingConfig, StderrOutput};
    let (primary_storage, logs_yaml) = logging::load_config_sections();
    let mut cfg = LoggingConfig::for_service("hs-distill-server")
        .with_stderr(StderrOutput::EnvFilter("info".into()));
    logs_yaml.apply_to(&mut cfg);
    let mut handle = match logging::init(cfg) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hs-distill-server: logging init failed: {e:#}");
            return None;
        }
    };
    if let Some(storage_cfg) = primary_storage {
        if let Ok(storage) = logging::build_logs_storage(&storage_cfg, &logs_yaml.bucket).await {
            let _ = handle.spawn_shipper(storage);
        }
    }
    Some(handle)
}
