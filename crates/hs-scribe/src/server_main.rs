use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;

use hs_scribe::config::AppConfig;
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
    // Secrets are exported into the environment, which is only sound while
    // this is the only thread: load them before the runtime (and its worker
    // threads) exist, and refuse to start if they cannot be read.
    hs_common::secrets::load_default_secrets().context("loading ~/.home-still/secrets.env")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let logging_handle = install_logging().await;
    let args = Args::parse();

    // No fallback: a malformed `scribe_server:` section, a bad HS_SCRIBE_*
    // value or a setting the server cannot run with stops the start (before
    // anything heavy is initialised).
    let config = AppConfig::load().context("loading the scribe server configuration")?;

    // Every route but /health and /readiness requires this secret; there is
    // no unauthenticated mode, and the server binds all interfaces by
    // default. Checked before anything is loaded: a host without the token
    // must not spend a model load discovering it.
    let token = hs_scribe::server::backend_token(|name| std::env::var(name))?;

    // libpdfium counts every PDF's pages (and renders them on Legacy hosts):
    // the server does not start without it, on any converter.
    hs_scribe::pdfium::require().map_err(|e| {
        anyhow::anyhow!(
            "hs-scribe-server requires libpdfium, which cannot be bound: {e:#}. Install it on the \
             system library path, or drop it into ~/.local/lib or ~/.home-still/dyld-libs"
        )
    })?;

    // A poisoned pdfium lock or a pdfium call that never returns cannot be
    // recovered in-process: health goes red, then the process exits.
    hs_scribe::pdfium::install_fault_exit(std::time::Duration::from_secs(10));

    // libonnxruntime defaults to "warning" verbosity, which floods the log with
    // shape-inference noise (logical_and_0.tmp_0.0, fill_constant_27.tmp_0.0)
    // for every page. The only API in ort 2.0.0-rc.11 to silence this on the
    // global env is `Environment::set_log_level`; `get_environment()` lazily
    // commits if needed, so this also serves as the single ort init point.
    if let Ok(env) = ort::environment::get_environment() {
        env.set_log_level(ort::logging::LogLevel::Error);
    }

    let backend_url = match config.backend {
        hs_scribe::config::BackendChoice::OpenAi => &config.openai_url,
        hs_scribe::config::BackendChoice::Ollama => &config.ollama_url,
        hs_scribe::config::BackendChoice::Cloud => &config.cloud_url,
    };
    tracing::info!(
        "Backend: {:?}, Backend URL: {}, Model: {}, VLM concurrency: {}",
        config.backend,
        backend_url,
        config.model,
        config.vlm_concurrency
    );
    // `ServerState::new` validates the effective config (a zero concurrency
    // would hang every conversion) and builds only what the configured
    // converter uses: the ONNX pipeline for Legacy, nothing for olmocr. It
    // fails the start if the Legacy pipeline cannot be built.
    let state = Arc::new(ServerState::new(config)?);

    let addr = format!("{}:{}", args.host, args.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let serve = axum::serve(listener, app(state, token));
    let result = serve.await;

    let _ = logging_handle.shutdown().await;
    result?;
    Ok(())
}

async fn install_logging() -> hs_common::logging::LoggingHandle {
    use hs_common::logging::{self, StderrOutput};
    const SERVICE: &str = "hs-scribe-server";
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
