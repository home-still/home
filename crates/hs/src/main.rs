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
mod installer;
mod mcp_client;
mod mcp_cmd;
mod migrate_cmd;
mod openalex_cmd;
mod pipeline_cmd;
mod restart_cmd;
mod scribe_cmd;
mod scribe_inbox;
mod scribe_inbox_install;
mod scribe_pool;
mod serve_cmd;
mod shutdown;
mod status_cmd;
#[cfg(test)]
mod test_http;
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

    let (service, force_info_stderr) = match &cli.command {
        TopCmd::Scribe {
            command: scribe_cmd::ScribeCmd::WatchEvents { .. },
        } => ("hs-scribe-watch", true),
        TopCmd::Distill {
            command: hs_distill::cli::DistillCmd::WatchEvents { .. },
        } => ("hs-distill-watch", true),
        _ => ("hs", false),
    };

    let (primary_storage, logs_yaml) = logging::load_config_sections();

    let stderr_output = if force_info_stderr && !cli.global.quiet {
        // Long-running daemons whose only signal is periodic INFO logs
        // (tick/measure/decide) need stderr at "info", regardless of
        // whether the operator remembered the global --verbose flag.
        // systemd-journal captures stderr on this unit, so anything
        // dropped here is lost forever.
        StderrOutput::EnvFilter("info".into())
    } else {
        StderrOutput::VerboseQuiet {
            verbose: cli.global.verbose,
            quiet: cli.global.quiet,
        }
    };
    let mut cfg = LoggingConfig::for_service(service).with_stderr(stderr_output);
    logs_yaml.apply_to(&mut cfg).unwrap_or_else(|e| {
        eprintln!("{service}: {e}");
        std::process::exit(2)
    });

    let handle = logging::init(cfg);

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
        TopCmd::Personal { .. } => |_| ExitCode::FAILURE,
        TopCmd::Config { .. } => |_| ExitCode::FAILURE,
        TopCmd::Serve { .. } => |_| ExitCode::FAILURE,
        TopCmd::Scribe { .. } => |_| ExitCode::FAILURE,
        TopCmd::Distill { .. } => |_| ExitCode::FAILURE,
        TopCmd::Status => |_| ExitCode::FAILURE,
        TopCmd::Restart => |_| ExitCode::FAILURE,
        TopCmd::Upgrade { .. } => |_| ExitCode::FAILURE,
        TopCmd::Cloud { .. } => |_| ExitCode::FAILURE,
        TopCmd::Mcp { .. } => |_| ExitCode::FAILURE,
        TopCmd::Migrate { .. } => |_| ExitCode::FAILURE,
        TopCmd::Pipeline { .. } => |_| ExitCode::FAILURE,
        TopCmd::Openalex { .. } => |_| ExitCode::FAILURE,
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio  runtime");

    let reporter_for_closure = reporter.clone();
    let result = rt.block_on(async move {
        let reporter = reporter_for_closure;
        let mut logging_handle = logging_handle;
        logging_handle
            .start_shipping(primary_storage_cfg.as_ref(), &logs_bucket)
            .await;

        let work = async {
            match cli.command {
                TopCmd::Paper { command } => {
                    paper::commands::dispatch(command, &cli.global, &reporter, &styles, &mode).await
                }
                TopCmd::Personal { command } => personal::commands::dispatch(command)
                    .await
                    .map_err(|e| anyhow::anyhow!(e)),
                TopCmd::Config { action } => handle_config(action, &cli.global, &reporter).await,
                TopCmd::Serve { command } => serve_cmd::dispatch(command, &reporter).await,
                TopCmd::Scribe { command } => scribe_cmd::dispatch(command, &reporter).await,
                TopCmd::Distill { command } => {
                    distill_cmd::dispatch(command, &cli.global, &reporter).await
                }
                TopCmd::Status => status_cmd::run(&cli.global).await,
                TopCmd::Restart => restart_cmd::run(&reporter).await,
                TopCmd::Cloud { command } => cloud_cmd::dispatch(command, &reporter).await,
                TopCmd::Mcp { command } => mcp_cmd::dispatch(command, &reporter).await,
                TopCmd::Upgrade { check, force, pre } => {
                    upgrade_cmd::run(check, force, pre, &cli.global, &reporter).await
                }
                TopCmd::Migrate { command } => match command {
                    cli::MigrateAction::Sharding => migrate_cmd::run_sharding(&reporter).await,
                    cli::MigrateAction::MoveRootOrphans { dry_run, limit } => {
                        migrate_cmd::run_move_root_orphans(&reporter, dry_run, limit).await
                    }
                    cli::MigrateAction::QuarantineBadContent { dry_run, limit } => {
                        migrate_cmd::run_quarantine_bad_content(&reporter, dry_run, limit).await
                    }
                    cli::MigrateAction::DropLocalHtml { dry_run } => {
                        migrate_cmd::run_drop_local_html(&reporter, dry_run).await
                    }
                    cli::MigrateAction::CanonicalizeDoiStems {
                        dry_run,
                        limit,
                        server,
                    } => {
                        migrate_cmd::run_canonicalize_doi_stems(
                            &reporter,
                            dry_run,
                            limit,
                            server.as_deref(),
                        )
                        .await
                    }
                },
                TopCmd::Pipeline { command } => pipeline_cmd::dispatch(command, &reporter).await,
                TopCmd::Openalex { command } => openalex_cmd::dispatch(command).await,
            }
        };
        tokio::pin!(work);

        shutdown::install();
        let stop = shutdown::global();
        let work_result = tokio::select! {
            result = &mut work => result,
            _ = stop.wait() => {
                // A command that declared itself cooperative finishes the
                // item it is on, prints its summary and returns; give it the
                // grace period. Every other command is cancelled right away.
                // A second Ctrl+C exits immediately either way (`shutdown`).
                if stop.is_cooperative() {
                    match tokio::time::timeout(shutdown::GRACE, &mut work).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!(
                            "interrupted: the command did not stop within {}s",
                            shutdown::GRACE.as_secs()
                        )),
                    }
                } else {
                    // Restore terminal in case raw mode was enabled (e.g. watch attach)
                    let _ = crossterm::terminal::disable_raw_mode();
                    reporter.finish("");
                    Err(anyhow::anyhow!("interrupted"))
                }
            }
        };

        let _ = logging_handle.shutdown().await;
        work_result
    });
    // A worker stuck in a hung syscall (NFS) must not keep the process alive
    // after the command has finished or been interrupted.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));

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
                .with_prompt("S3 secret key for object storage (Enter to skip; required for Garage/S3 backend)")
                .allow_empty(true)
                .interact()?;

            std::fs::write(&config_path, generate_config(&email, &core_key))?;
            reporter.status("Created", &format!("{}", config_path.display()));

            if !s3_secret.is_empty() {
                let secrets_path = parent.join("secrets.env");
                write_private_file(
                    &secrets_path,
                    format!("HS_S3_SECRET_KEY={}\n", s3_secret).as_bytes(),
                )?;
                reporter.status("Created", &format!("{}", secrets_path.display()));
            }

            // Create project directory structure
            let project = hs_common::resolve_project_dir();
            std::fs::create_dir_all(project.join("papers").join("manually_downloaded"))?;
            std::fs::create_dir_all(project.join("markdown"))?;
            std::fs::create_dir_all(project.join("catalog"))?;

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

/// Write `contents` to `path` so the secret is never readable by anyone else:
/// the file is created with mode 0600, and a pre-existing file is truncated
/// and chmod'ed to 0600 BEFORE the secret is written into it.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("restrict {} to its owner: {e}", path.display()))?;
    }
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))
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

#[cfg(all(test, unix))]
mod tests {
    use super::write_private_file;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_new_secrets_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        write_private_file(&path, b"HS_S3_SECRET_KEY=s\n").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"HS_S3_SECRET_KEY=s\n");
    }

    /// `hs config init --force` over a world-readable secrets file: the new
    /// secret must never sit in a file anyone else can read.
    #[test]
    fn an_existing_world_readable_file_is_restricted_before_the_secret_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, b"old contents that are longer than the new ones\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_file(&path, b"HS_S3_SECRET_KEY=s\n").unwrap();

        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"HS_S3_SECRET_KEY=s\n");
    }

    #[test]
    fn failures_are_errors_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let missing_parent = dir.path().join("no-such-dir").join("secrets.env");
        assert!(write_private_file(&missing_parent, b"x").is_err());
    }
}
