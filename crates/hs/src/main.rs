use clap::Parser;
use dialoguer::{Confirm, Input};
use std::process::ExitCode;
use std::sync::Arc;

mod cli;

use cli::{Cli, TopCmd};
use hs_style::mode::{self, OutputMode};
use hs_style::reporter::{Reporter, SilentReporter};
use hs_style::styles::Styles;
use hs_style::tty_reporter::TtyReporter;

const DEFAULT_CONFIG: &str = include_str!("../config/default.yaml");

fn main() -> ExitCode {
    let cli = Cli::parse();

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
            OutputMode::Pipe => Arc::new(hs_style::pipe_reporter::PipeReporter),
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
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio  runtime");

    let result = rt.block_on(async {
        match cli.command {
            TopCmd::Paper { command } => {
                paper::commands::dispatch(command, &cli.global, &reporter, &styles).await
            }
            TopCmd::Config { action } => handle_config(action, &cli.global, &reporter).await,
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
    global: &hs_style::global_args::GlobalArgs,
    reporter: &std::sync::Arc<dyn hs_style::reporter::Reporter>,
) -> anyhow::Result<()> {
    match action {
        cli::ConfigAction::Init { force } => {
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
            let config_path = home.join(".home-still/config.yaml");

            if config_path.exists() && !force {
                let overwrite = Confirm::new()
                    .with_prompt("Config already exists.  Overwrite?")
                    .default(false)
                    .interact()?;

                if !overwrite {
                    reporter.status("Skipped", "config unchanged");
                    return Ok(());
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
            std::fs::write(&config_path, generate_config(&email))?;
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
            let path = home.join(".home-still/config.yaml");
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn generate_config(email: &str) -> String {
    let mut content = DEFAULT_CONFIG.to_string();
    if !email.is_empty() {
        content = content.replace(
            "# unpaywall_email: you@example.com",
            &format!("unpaywall_email: {}", email),
        );
    }
    content
}
