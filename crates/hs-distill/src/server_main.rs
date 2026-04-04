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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
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
    axum::serve(listener, server::app(state)).await?;

    Ok(())
}
