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
    #[arg(long, default_value = "7433")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let logging_handle = install_logging().await;
    let args = Args::parse();

    let config = AppConfig::load().unwrap_or_else(|e| {
        tracing::warn!("Config load error: {e}, using defaults");
        AppConfig::default()
    });

    tracing::info!(
        "Backend: {:?}, Ollama URL: {}, Model: {}, VLM concurrency: {}",
        config.backend,
        config.ollama_url,
        config.model,
        config.vlm_concurrency
    );
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
    let serve = axum::serve(listener, app(state));
    let result = serve.await;

    if let Some(h) = logging_handle {
        let _ = h.shutdown().await;
    }
    result?;
    Ok(())
}

async fn install_logging() -> Option<hs_common::logging::LoggingHandle> {
    use hs_common::logging::{self, LoggingConfig, StderrOutput};
    let (primary_storage, logs_yaml) = logging::load_config_sections();
    let mut cfg = LoggingConfig::for_service("hs-scribe-server")
        .with_stderr(StderrOutput::EnvFilter("info".into()));
    logs_yaml.apply_to(&mut cfg);
    let mut handle = match logging::init(cfg) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hs-scribe-server: logging init failed: {e:#}");
            return None;
        }
    };
    if let Some(storage_cfg) = primary_storage {
        if let Ok(storage) = logging::build_logs_storage(&storage_cfg, &logs_yaml.bucket) {
            let _ = handle.spawn_shipper(storage);
        }
    }
    Some(handle)
}
