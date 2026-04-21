use anyhow::Result;
use clap::Parser;
use std::sync::atomic::{AtomicU64, AtomicUsize};
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

fn main() -> Result<()> {
    // Must run before ANY dlopen or tokio init — re-execs self with the
    // platform's dynamic-lib search path augmented so ort's CUDA provider
    // (Linux) and pdfium (macOS) load from our bundled directories
    // instead of the system default.
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
        last_conversion_ms: Arc::new(AtomicU64::new(0)),
        total_conversions: Arc::new(AtomicU64::new(0)),
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
        if let Ok(storage) = logging::build_logs_storage(&storage_cfg, &logs_yaml.bucket).await {
            let _ = handle.spawn_shipper(storage);
        }
    }
    Some(handle)
}
