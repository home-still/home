use clap::Parser;
use dialoguer::{Confirm, Input};
use hs_common::CONFIG_REL_PATH;
use std::process::ExitCode;
use std::sync::Arc;

pub use hs_common::catalog;
mod cli;
mod cloud_cmd;
pub mod daemon;
mod distill_cmd;
mod mcp_client;
mod mcp_cmd;
mod migrate_cmd;
mod restart_cmd;
mod scribe_cmd;
mod scribe_pool;
mod serve_cmd;
mod server_cmd;
mod status_cmd;
mod upgrade_cmd;

use cli::{Cli, TopCmd};
use hs_common::mode::{self, OutputMode};
use hs_common::reporter::{Reporter, SilentReporter};
use hs_common::styles::Styles;
use hs_common::tty_reporter::TtyReporter;

const DEFAULT_CONFIG: &str = include_str!("../config/default.yaml");

fn init_logging(
    cli: &Cli,
) -> (
    hs_common::logging::LoggingHandle,
    Option<hs_common::storage::StorageConfig>,
    String,
) {
    use hs_common::logging::{self, LoggingConfig, StderrOutput};

    let service = match &cli.command {
        TopCmd::Scribe {
            command: scribe_cmd::ScribeCmd::WatchEvents { .. },
        } => "hs-scribe-watch",
        TopCmd::Distill {
            command: hs_distill::cli::DistillCmd::WatchEvents { .. },
        } => "hs-distill-watch",
        _ => "hs",
    };

    let (primary_storage, logs_yaml) = logging::load_config_sections();

    let mut cfg = LoggingConfig::for_service(service).with_stderr(StderrOutput::VerboseQuiet {
        verbose: cli.global.verbose,
        quiet: cli.global.quiet,
    });
    logs_yaml.apply_to(&mut cfg);

    let handle = logging::init(cfg).expect("install logging subscriber");

    (handle, primary_storage, logs_yaml.bucket)
}

fn main() -> ExitCode {
    let _ = hs_common::secrets::load_default_secrets();
    let cli = Cli::parse();

    let (logging_handle, primary_storage_cfg, logs_bucket) = init_logging(&cli);

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
        TopCmd::Serve { .. } => |_| ExitCode::FAILURE,
        TopCmd::Server { .. } => |_| ExitCode::FAILURE,
        TopCmd::Scribe { .. } => |_| ExitCode::FAILURE,
        TopCmd::Distill { .. } => |_| ExitCode::FAILURE,
        TopCmd::Status => |_| ExitCode::FAILURE,
        TopCmd::Restart => |_| ExitCode::FAILURE,
        TopCmd::Upgrade { .. } => |_| ExitCode::FAILURE,
        TopCmd::Cloud { .. } => |_| ExitCode::FAILURE,
        TopCmd::Mcp { .. } => |_| ExitCode::FAILURE,
        TopCmd::Migrate { .. } => |_| ExitCode::FAILURE,
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio  runtime");

    let reporter_for_closure = reporter.clone();
    let result = rt.block_on(async move {
        let reporter = reporter_for_closure;
        let mut logging_handle = logging_handle;
        if let Some(primary_cfg) = primary_storage_cfg {
            if let Ok(storage) =
                hs_common::logging::build_logs_storage(&primary_cfg, &logs_bucket).await
            {
                let _ = logging_handle.spawn_shipper(storage);
            }
        }

        let work = async {
            match cli.command {
                TopCmd::Paper { command } => {
                    let is_download = matches!(&command, paper::cli::PaperCmd::Download { .. });
                    let result =
                        paper::commands::dispatch(command, &cli.global, &reporter, &styles, &mode)
                            .await;
                    // Auto-trigger: start scribe watcher after successful download
                    if is_download && result.is_ok() {
                        scribe_cmd::ensure_watcher_running(&reporter);
                    }
                    result
                }
                TopCmd::Config { action } => handle_config(action, &cli.global, &reporter).await,
                TopCmd::Serve { command } => serve_cmd::dispatch(command, &reporter).await,
                TopCmd::Server { command } => server_cmd::dispatch(command, &reporter).await,
                TopCmd::Scribe { command } => scribe_cmd::dispatch(command, &reporter).await,
                TopCmd::Distill { command } => {
                    distill_cmd::dispatch(command, &cli.global, &reporter).await
                }
                TopCmd::Status => status_cmd::run().await,
                TopCmd::Restart => restart_cmd::run(&reporter).await,
                TopCmd::Cloud { command } => cloud_cmd::dispatch(command, &reporter).await,
                TopCmd::Mcp { command } => mcp_cmd::dispatch(command, &reporter).await,
                TopCmd::Upgrade { check, force, pre } => {
                    upgrade_cmd::run(check, force, pre, &cli.global, &reporter).await
                }
                TopCmd::Migrate { command } => match command {
                    cli::MigrateAction::Sharding => migrate_cmd::run_sharding(&reporter).await,
                },
            }
        };

        let work_result = tokio::select! {
            result = work => result,
            _ = tokio::signal::ctrl_c() => {
                // Restore terminal in case raw mode was enabled (e.g. watch attach)
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.finish("");
                Err(anyhow::anyhow!("interrupted"))
            }
        };

        let _ = logging_handle.shutdown().await;
        work_result
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
            let s3_secret: String = Input::new()
                .with_prompt("S3 secret key for object storage (Enter to skip; required for MinIO/S3 backend)")
                .allow_empty(true)
                .interact()?;

            std::fs::write(&config_path, generate_config(&email, &core_key))?;
            reporter.status("Created", &format!("{}", config_path.display()));

            if !s3_secret.is_empty() {
                let secrets_path = parent.join("secrets.env");
                std::fs::write(&secrets_path, format!("HS_S3_SECRET_KEY={}\n", s3_secret))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &secrets_path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
                reporter.status("Created", &format!("{}", secrets_path.display()));
            }

            // Create project directory structure
            let project = hs_common::resolve_project_dir();
            let _ = std::fs::create_dir_all(project.join("papers").join("manually_downloaded"));
            let _ = std::fs::create_dir_all(project.join("markdown"));
            let _ = std::fs::create_dir_all(project.join("catalog"));

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
