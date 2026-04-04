use clap::Parser;
use dialoguer::{Confirm, Input};
use hs_common::CONFIG_REL_PATH;
use std::process::ExitCode;
use std::sync::Arc;

pub use hs_common::catalog;
mod cli;
pub mod daemon;
mod distill_cmd;
mod scribe_cmd;
mod scribe_pool;

use cli::{Cli, TopCmd};
use hs_common::mode::{self, OutputMode};
use hs_common::reporter::{Reporter, SilentReporter};
use hs_common::styles::Styles;
use hs_common::tty_reporter::TtyReporter;

const DEFAULT_CONFIG: &str = include_str!("../config/default.yaml");

fn init_logging(verbose: bool, quiet: bool) {
    use tracing_subscriber::{
        fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    let log_dir = hs_common::resolve_log_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    // Stderr layer: human-readable, respects --verbose/--quiet
    let stderr_filter = if quiet {
        "error"
    } else if verbose {
        "debug"
    } else {
        "warn"
    };
    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::new(stderr_filter));

    // File layer: always INFO+, appended to hs.log
    let file_appender = tracing_appender::rolling::never(&log_dir, "hs.log");
    let file_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(file_appender)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    init_logging(cli.global.verbose, cli.global.quiet);

    let mode = mode::detect(cli.global.color_str(), cli.global.is_json());

    match mode {
        OutputMode::Rich => owo_colors::set_override(true),
        _ => owo_colors::set_override(false),
    }

    let reporter: Arc<dyn Reporter> = if cli.global.quiet {
        Arc::new(SilentReporter)
    } else {
        match mode {
            OutputMode::Rich => Arc::new(TtyReporter::new(true)),
            OutputMode::Plain => Arc::new(TtyReporter::new(false)),
            OutputMode::Pipe => Arc::new(hs_common::pipe_reporter::PipeReporter),
        }
    };

    let styles = match mode {
        OutputMode::Rich => Styles::colored(),
        _ => Styles::plain(),
    };

    // Capture exit code mapper before cli.command is moved
    let exit_code_mapper: fn(&anyhow::Error) -> ExitCode = match &cli.command {
        TopCmd::Paper { .. } => paper::exit_codes::from_error,
        TopCmd::Config { .. } => |_| ExitCode::FAILURE,
        TopCmd::Scribe { .. } => |_| ExitCode::FAILURE,
        TopCmd::Distill { .. } => |_| ExitCode::FAILURE,
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio  runtime");

    let result = rt.block_on(async {
        let work = async {
            match cli.command {
                TopCmd::Paper { command } => {
                    paper::commands::dispatch(command, &cli.global, &reporter, &styles, &mode).await
                }
                TopCmd::Config { action } => handle_config(action, &cli.global, &reporter).await,
                TopCmd::Scribe { command } => scribe_cmd::dispatch(command, &reporter).await,
                TopCmd::Distill { command } => distill_cmd::dispatch(command, &reporter).await,
            }
        };

        tokio::select! {
            result = work => result,
            _ = tokio::signal::ctrl_c() => {
                // Restore terminal in case raw mode was enabled (e.g. watch attach)
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.finish("");
                Err(anyhow::anyhow!("interrupted"))
            }
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            reporter.error(&format!("Error: {e:#}"));
            exit_code_mapper(&e)
        }
    }
}

async fn handle_config(
    action: cli::ConfigAction,
    global: &hs_common::global_args::GlobalArgs,
    reporter: &std::sync::Arc<dyn hs_common::reporter::Reporter>,
) -> anyhow::Result<()> {
    match action {
        cli::ConfigAction::Init { force } => {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
            let config_path = home.join(CONFIG_REL_PATH);

            if config_path.exists() && !force {
                if global.yes {
                    // --yes: overwrite without asking
                } else {
                    let overwrite = Confirm::new()
                        .with_prompt("Config already exists.  Overwrite?")
                        .default(false)
                        .interact()?;

                    if !overwrite {
                        reporter.status("Skipped", "config unchanged");
                        return Ok(());
                    }
                }
            }

            let parent = config_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?;
            std::fs::create_dir_all(parent)?;
            let email: String = Input::new()
                .with_prompt("Email for Unpaywall API (enables more downloads, Enter to skip)")
                .allow_empty(true)
                .interact()?;
            let core_key: String = Input::new()
                .with_prompt("CORE API key (https://core.ac.uk, Enter to skip)")
                .allow_empty(true)
                .interact()?;

            std::fs::write(&config_path, generate_config(&email, &core_key))?;
            reporter.status("Created", &format!("{}", config_path.display()));

            Ok(())
        }

        cli::ConfigAction::Show => {
            let config = paper::config::Config::load()?;
            if global.is_json() {
                let json = serde_json::to_string_pretty(&config)?;
                println!("{json}");
            } else {
                let yaml = serde_yaml_ng::to_string(&config)?;
                println!("{yaml}");
            }
            Ok(())
        }

        cli::ConfigAction::Path => {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
            let path = home.join(CONFIG_REL_PATH);
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn generate_config(email: &str, core_key: &str) -> String {
    let mut content = DEFAULT_CONFIG.to_string();
    if !email.is_empty() {
        content = content.replace(
            "# unpaywall_email: you@example.com",
            &format!("unpaywall_email: {}", email),
        );
    }
    if !core_key.is_empty() {
        content = content.replace(
            "# core_api_key: your-key-here",
            &format!("core_api_key: {}", core_key),
        );
    }
    content
}
