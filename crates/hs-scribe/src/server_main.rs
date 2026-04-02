use anyhow::Result;
use clap::Parser;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use hs_scribe::config::AppConfig;
use hs_scribe::pipeline::processor::Processor;
use hs_scribe::server::{app, ServerState};

#[derive(Parser)]
#[command(name = "hs-scribe-server")]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    #[arg(long, default_value = "7432")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let config = AppConfig::load().unwrap_or_else(|e| {
        tracing::warn!("Config load error: {e}, using defaults");
        AppConfig::default()
    });

    let vlm_sem = Arc::new(tokio::sync::Semaphore::new(config.vlm_concurrency));
    let processor = Processor::new(config.clone())?;
    let state = Arc::new(ServerState {
        processor,
        config,
        vlm_sem,
        in_flight: Arc::new(AtomicUsize::new(0)),
    });

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app(state)).await?;

    Ok(())
}
