use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::reporter::Reporter;

const DEFAULT_SCRIBE_PORT: u16 = 7433;
const DEFAULT_DISTILL_PORT: u16 = 7434;
const DEFAULT_MCP_PORT: u16 = 7445;

#[derive(Subcommand, Debug)]
pub enum ServeCmd {
    /// Run a scribe server (auto-init, foreground)
    Scribe {
        /// Action: start (background), stop, or omit for foreground
        action: Option<ServeAction>,
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_SCRIBE_PORT)]
        port: u16,
        /// Install as a system service (systemd on Linux, launchd on macOS) and start it
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop and remove the system service
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Run a distill server (auto-init, foreground)
    Distill {
        /// Action: start (background), stop, or omit for foreground
        action: Option<ServeAction>,
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_DISTILL_PORT)]
        port: u16,
        /// Install as a system service and start it
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop and remove the system service
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Run an MCP server (foreground)
    Mcp {
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_MCP_PORT)]
        port: u16,
        /// Install as a system service and start it
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop and remove the system service
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// NATS event-watch daemon that converts `papers.ingested` events into
    /// markdown via the scribe pool. Runs as a user-level service.
    ScribeWatch {
        /// Install as a user service (systemd --user on Linux, LaunchAgent on
        /// macOS) and start it. Runs under the current user — no sudo.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop and remove the user service
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// NATS event-watch daemon that indexes `scribe.completed` events into
    /// Qdrant via the distill server. Runs as a user-level service.
    DistillWatch {
        /// Install as a user service and start it
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop and remove the user service
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ServeAction {
    /// Start services in the background
    Start,
    /// Stop running services
    Stop,
}

pub async fn dispatch(cmd: ServeCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        // -- install / uninstall --
        ServeCmd::Scribe {
            install: true,
            port,
            ..
        } => install_service("scribe", port, reporter).await,
        ServeCmd::Distill {
            install: true,
            port,
            ..
        } => install_service("distill", port, reporter).await,
        ServeCmd::Mcp {
            install: true,
            port,
            ..
        } => install_service("mcp", port, reporter).await,
        ServeCmd::Scribe {
            uninstall: true, ..
        } => uninstall_service("scribe", reporter).await,
        ServeCmd::Distill {
            uninstall: true, ..
        } => uninstall_service("distill", reporter).await,
        ServeCmd::Mcp {
            uninstall: true, ..
        } => uninstall_service("mcp", reporter).await,
        ServeCmd::ScribeWatch { install: true, .. } => {
            install_user_service(
                "scribe-watch-events",
                &["scribe", "watch-events"],
                "Home-Still scribe event-watch daemon (NATS papers.ingested → scribe pool)",
                reporter,
            )
            .await
        }
        ServeCmd::DistillWatch { install: true, .. } => {
            install_user_service(
                "distill-watch-events",
                &["distill", "watch-events"],
                "Home-Still distill event-watch daemon (NATS scribe.completed → distill index)",
                reporter,
            )
            .await
        }
        ServeCmd::ScribeWatch {
            uninstall: true, ..
        } => uninstall_user_service("scribe-watch-events", reporter).await,
        ServeCmd::DistillWatch {
            uninstall: true, ..
        } => uninstall_user_service("distill-watch-events", reporter).await,
        ServeCmd::ScribeWatch { .. } => serve_scribe_watch(reporter).await,
        ServeCmd::DistillWatch { .. } => serve_distill_watch(reporter).await,

        // -- start / stop (background) --
        ServeCmd::Scribe {
            action: Some(ServeAction::Start),
            ..
        } => crate::scribe_cmd::cmd_server(crate::scribe_cmd::ServerAction::Start).await,
        ServeCmd::Scribe {
            action: Some(ServeAction::Stop),
            ..
        } => crate::scribe_cmd::cmd_server(crate::scribe_cmd::ServerAction::Stop).await,
        ServeCmd::Distill {
            action: Some(ServeAction::Start),
            ..
        } => crate::distill_cmd::cmd_server_start(reporter).await,
        ServeCmd::Distill {
            action: Some(ServeAction::Stop),
            ..
        } => crate::distill_cmd::cmd_server_stop(reporter).await,

        // -- foreground (default, no action) --
        ServeCmd::Scribe { port, .. } => {
            check_system_service_conflict("scribe")?;
            serve_scribe(port, reporter).await
        }
        ServeCmd::Distill { port, .. } => {
            check_system_service_conflict("distill")?;
            serve_distill(port, reporter).await
        }
        ServeCmd::Mcp { port, .. } => {
            check_system_service_conflict("mcp")?;
            serve_mcp(port, reporter).await
        }
    }
}

// ── Scribe ─────────────────────────────────────────────────────

async fn serve_scribe(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = hs_scribe::config::ScribeConfig::load()?;
    if !cfg.local_server {
        anyhow::bail!(
            "local_server is disabled in scribe config. \
             This machine is configured as a client-only node.\n\
             Set scribe.local_server: true in ~/.home-still/config.yaml to enable."
        );
    }

    reporter.status("Serve", &format!("scribe on port {port}"));

    // Start server (foreground — blocks until shutdown)
    reporter.status("Start", "starting scribe server");
    let result = super::scribe_cmd::start_server_foreground(port, reporter).await;

    reporter.finish("scribe server stopped");
    result
}

// ── Distill ────────────────────────────────────────────────────

async fn serve_distill(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", &format!("distill on port {port}"));

    // Auto-init (idempotent)
    reporter.status("Init", "checking distill prerequisites");
    super::distill_cmd::ensure_init(reporter).await?;

    // Start server (foreground — blocks until shutdown)
    reporter.status("Start", "starting distill server");
    let result = super::distill_cmd::start_server_foreground(port, reporter).await;

    reporter.finish("distill server stopped");
    result
}

// ── MCP ────────────────────────────────────────────────────────

async fn serve_mcp(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", &format!("mcp on port {port}"));

    let binary = find_mcp_binary()?.ok_or_else(|| {
        anyhow::anyhow!(
            "hs-mcp binary not found. Build with:\n  \
             HS_RELEASE_TAG=<tag> cargo build --release -p hs-mcp"
        )
    })?;

    let addr = format!("0.0.0.0:{port}");

    reporter.status("Start", &format!("hs-mcp --serve {addr}"));

    // Ctrl+C / SIGTERM (e.g. `systemctl stop`) reach us as a graceful stop
    // request, so the child can be told to shut down before `main` ends us.
    let stop = crate::shutdown::cooperative();

    let mut child = tokio::process::Command::new(&binary)
        .args(["--serve", &addr])
        .spawn()
        .context("Failed to start hs-mcp")?;

    // Wait for either child exit or a stop request
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = stop.wait() => {
            // Forward SIGTERM to the child for graceful shutdown
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe { libc::kill(pid as i32, libc::SIGTERM); }
            }
            // Wait up to 5 seconds for graceful exit, then kill
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                child.wait(),
            ).await {
                Ok(Ok(s)) => s,
                _ => { child.kill().await.ok(); child.wait().await? }
            }
        }
    };

    // An operator-requested stop is not a failure, whatever the child's
    // status after SIGTERM looks like.
    if stop.requested() {
        reporter.finish("mcp server stopped");
        return Ok(());
    }
    if !status.success() {
        anyhow::bail!("hs-mcp exited with {status}");
    }

    reporter.finish("mcp server stopped");
    Ok(())
}

// ── Watchers (user-level services) ──────────────────────────────

async fn serve_scribe_watch(reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", "scribe watch-events");
    crate::scribe_cmd::cmd_watch_events(None, reporter).await
}

async fn serve_distill_watch(reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", "distill watch-events");
    crate::distill_cmd::cmd_watch_events(None, reporter).await
}

/// Install a user-level systemd unit (Linux) or LaunchAgent (macOS) that
/// runs `hs <exec_args...>` under the invoking user. Used for the event-
/// watch daemons — they need the user's NATS creds and S3 secrets, so a
/// root-owned system unit isn't appropriate.
async fn install_user_service(
    service_name: &str,
    exec_args: &[&str],
    description: &str,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (service_name, exec_args, description, reporter);
        anyhow::bail!("--install is only supported on Linux and macOS");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let hs_bin = std::env::current_exe().context("Cannot find hs binary path")?;
        #[cfg(target_os = "macos")]
        let hs_path = hs_bin.display().to_string();
        #[cfg(target_os = "linux")]
        let home_dir =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
        #[cfg(target_os = "linux")]
        let secrets_path = home_dir.join(".home-still").join("secrets.env");

        #[cfg(target_os = "linux")]
        {
            let unit_dir = home_dir.join(".config/systemd/user");
            std::fs::create_dir_all(&unit_dir)?;
            let unit_path = unit_dir.join(format!("hs-{service_name}.service"));

            let unit = render_user_unit(
                description,
                &home_dir,
                &hs_bin,
                exec_args,
                secrets_path.exists().then_some(secrets_path.as_path()),
            );

            reporter.status("Install", &format!("{}", unit_path.display()));
            std::fs::write(&unit_path, &unit).context("Failed to write user unit file")?;

            let status = tokio::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status()
                .await
                .context("systemctl --user daemon-reload failed")?;
            if !status.success() {
                anyhow::bail!("systemctl --user daemon-reload failed");
            }
            let full_name = format!("hs-{service_name}.service");
            run_checked(
                tokio::process::Command::new("systemctl").args(["--user", "enable", &full_name]),
                &format!("systemctl --user enable {full_name}"),
            )
            .await?;
            // `enable --now` leaves an instance that is already running on
            // the old unit; a re-install must pick up the rewritten one.
            run_checked(
                tokio::process::Command::new("systemctl").args(["--user", "restart", &full_name]),
                &format!("systemctl --user restart {full_name}"),
            )
            .await?;

            reporter.finish(&format!(
                "Installed and started {full_name}\n\
             View logs: journalctl --user -u {full_name} -f\n\
             Stop:      systemctl --user stop {full_name}\n\
             Disable:   systemctl --user disable {full_name}"
            ));
        }

        #[cfg(target_os = "macos")]
        {
            let _ = description;
            let label = format!("com.home-still.{service_name}");
            let mut program_args = vec![hs_path.clone()];
            program_args.extend(exec_args.iter().map(|a| a.to_string()));
            let (plist_path, log_path) = write_launchd_agent(&label, service_name, &program_args)?;
            reporter.status("Install", &format!("{}", plist_path.display()));

            let _ = tokio::process::Command::new("launchctl")
                .args(["unload", &plist_path.to_string_lossy()])
                .status()
                .await;
            let status = tokio::process::Command::new("launchctl")
                .args(["load", &plist_path.to_string_lossy()])
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("launchctl load failed");
            }

            reporter.finish(&format!(
                "Installed and started {label}\n\
             View logs: tail -f {}\n\
             Stop:      launchctl unload {}\n\
             Remove:    rm {}",
                log_path.display(),
                plist_path.display(),
                plist_path.display()
            ));
        }

        Ok(())
    }
}

async fn uninstall_user_service(service_name: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (service_name, reporter);
        anyhow::bail!("--uninstall is only supported on Linux and macOS");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let home_dir =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;

        #[cfg(target_os = "linux")]
        {
            let full_name = format!("hs-{service_name}.service");
            let unit_path = home_dir.join(".config/systemd/user").join(&full_name);
            if !unit_path.exists() {
                reporter.finish(&format!("{full_name} is not installed"));
                return Ok(());
            }
            reporter.status("Stop", &full_name);
            run_checked(
                tokio::process::Command::new("systemctl")
                    .args(["--user", "disable", "--now", &full_name]),
                &format!("systemctl --user disable --now {full_name}"),
            )
            .await?;
            std::fs::remove_file(&unit_path)
                .with_context(|| format!("removing {}", unit_path.display()))?;
            run_checked(
                tokio::process::Command::new("systemctl").args(["--user", "daemon-reload"]),
                "systemctl --user daemon-reload",
            )
            .await?;
            reporter.finish(&format!("Removed {full_name}"));
        }

        #[cfg(target_os = "macos")]
        {
            let label = format!("com.home-still.{service_name}");
            let plist_path = home_dir
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            remove_launch_agent(&label, &plist_path, reporter).await?;
        }

        Ok(())
    }
}

// ── Unit / plist generation ────────────────────────────────────
//
// Generators and writers are separate from the `sudo` / `systemctl` /
// `launchctl` calls so they can be exercised without touching the host.

/// The user a system unit runs as: `$USER`, else `id -un`. There is no
/// default — a unit running as the wrong account is worse than no unit.
#[cfg(any(target_os = "linux", test))]
fn resolve_user(
    env_user: Option<String>,
    id_un: impl FnOnce() -> Option<String>,
) -> Result<String> {
    let user = env_user
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .or_else(|| {
            id_un()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
        })
        .context("cannot determine the current user: $USER is unset and `id -un` failed")?;
    // `User=` is one unit-file line: refuse anything that could break out of it.
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!("refusing to write User={user:?} into a systemd unit");
    }
    Ok(user)
}

#[cfg(target_os = "linux")]
fn current_user() -> Result<String> {
    resolve_user(std::env::var("USER").ok(), || {
        let out = std::process::Command::new("id").arg("-un").output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
}

/// Text of the root-installed `/etc/systemd/system/hs-serve-<kind>.service`.
#[cfg(any(target_os = "linux", test))]
fn render_system_unit(
    service_type: &str,
    port: u16,
    user: &str,
    home: &std::path::Path,
    hs_bin: &std::path::Path,
    fastembed_cache: &std::path::Path,
    secrets_env: Option<&std::path::Path>,
) -> String {
    let env_file_line = secrets_env
        .map(|p| format!("EnvironmentFile=-{}\n", p.display()))
        .unwrap_or_default();
    format!(
        r#"[Unit]
Description=Home-Still {service_type} server
After=network.target

[Service]
Type=simple
User={user}
WorkingDirectory={home}
{env_file_line}Environment=FASTEMBED_CACHE_PATH={cache}
ExecStart={hs_path} serve {service_type} --port {port}
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
"#,
        home = home.display(),
        cache = fastembed_cache.display(),
        hs_path = hs_bin.display(),
    )
}

/// Text of the per-user `hs-<name>.service` that runs a watch-events daemon.
/// Those commands exit non-zero when their event stream ends, which
/// `Restart=always` turns into a restart after `RestartSec`.
#[cfg(any(target_os = "linux", test))]
fn render_user_unit(
    description: &str,
    home: &std::path::Path,
    hs_bin: &std::path::Path,
    exec_args: &[&str],
    secrets_env: Option<&std::path::Path>,
) -> String {
    let env_file_line = secrets_env
        .map(|p| format!("EnvironmentFile=-{}\n", p.display()))
        .unwrap_or_default();
    format!(
        r#"[Unit]
Description={description}
After=network.target

[Service]
Type=simple
WorkingDirectory={home}
{env_file_line}ExecStart={hs_path} {exec_spaced}
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
"#,
        home = home.display(),
        hs_path = hs_bin.display(),
        exec_spaced = exec_args.join(" "),
    )
}

/// Install `contents` at `dest` as root without staging it in a
/// world-writable directory: the text goes to `sudo tee` on stdin, so there
/// is no predictable temp path to race or symlink.
#[cfg(target_os = "linux")]
async fn sudo_write_file(dest: &str, contents: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("sudo")
        .args(["tee", dest])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("sudo tee could not run")?;
    let mut stdin = child.stdin.take().context("sudo tee has no stdin")?;
    stdin
        .write_all(contents.as_bytes())
        .await
        .with_context(|| format!("writing {dest} via sudo tee"))?;
    drop(stdin);
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("Failed to install {dest} (sudo tee exited {status})");
    }
    Ok(())
}

/// Run `cmd` to completion. A spawn failure or a non-zero exit is an error
/// naming `what`: install and uninstall must not report success over a
/// `systemctl` / `sudo` that did nothing.
#[cfg(target_os = "linux")]
async fn run_checked(cmd: &mut tokio::process::Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .await
        .with_context(|| format!("{what} could not run"))?;
    if !status.success() {
        anyhow::bail!("{what} failed ({status})");
    }
    Ok(())
}

/// Unload a LaunchAgent and delete its plist. A job that was not loaded makes
/// `launchctl unload` fail harmlessly, so that is a warning; a plist that
/// cannot be removed is an error. No plist at all means nothing is installed.
#[cfg(target_os = "macos")]
async fn remove_launch_agent(
    label: &str,
    plist_path: &std::path::Path,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    if !plist_path.exists() {
        reporter.finish(&format!("{label} is not installed"));
        return Ok(());
    }
    reporter.status("Unload", label);
    let status = tokio::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .status()
        .await
        .context("launchctl unload could not run")?;
    if !status.success() {
        reporter.warn(&format!(
            "`launchctl unload` exited {status}; {label} may not have been loaded"
        ));
    }
    std::fs::remove_file(plist_path)
        .with_context(|| format!("removing {}", plist_path.display()))?;
    reporter.finish(&format!("Removed {label}"));
    Ok(())
}

/// Escape the five XML-significant characters of a plist `<string>`/`<key>`.
#[cfg(any(target_os = "macos", test))]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// `KEY=value` lines of a `secrets.env`, comments and blanks dropped,
/// one layer of surrounding quotes removed.
#[cfg(any(target_os = "macos", test))]
fn parse_secrets_env(contents: &str) -> Vec<(String, String)> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim_matches('"').trim_matches('\'').to_string(),
            )
        })
        .collect()
}

/// Text of a KeepAlive LaunchAgent. Every interpolated value is XML-escaped.
/// `KeepAlive` restarts the job on any exit (including the non-zero exit of
/// a finished event stream), `ThrottleInterval` is the minimum gap.
#[cfg(any(target_os = "macos", test))]
fn render_launchd_plist(
    label: &str,
    program_args: &[String],
    secrets: &[(String, String)],
    log_path: &std::path::Path,
) -> String {
    let args = program_args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect::<String>();
    let env = secrets
        .iter()
        .map(|(k, v)| {
            format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect::<String>();
    let log = xml_escape(&log_path.to_string_lossy());
    let label = xml_escape(label);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://schemas.apple.com/dtds/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
{env}    </dict>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// `~/Library/Logs/home-still/hs-<name>.log`.
#[cfg(any(target_os = "macos", test))]
fn launchd_log_path(home: &std::path::Path, name: &str) -> PathBuf {
    home.join("Library/Logs/home-still")
        .join(format!("hs-{name}.log"))
}

/// Write `contents` to `path` atomically with mode 0600 (the plist carries
/// secrets): created owner-only in the destination directory, then renamed
/// into place, so no moment exists where it is world-readable.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn write_private_file(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .context("plist path has no parent directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a private temp file in {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(contents.as_bytes())?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Generate and write `~/Library/LaunchAgents/<label>.plist` (0600), creating
/// the per-user log directory. Returns the plist and log paths.
#[cfg(target_os = "macos")]
fn write_launchd_agent(
    label: &str,
    log_name: &str,
    program_args: &[String],
) -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    let plist_dir = home.join("Library/LaunchAgents");
    let plist_path = plist_dir.join(format!("{label}.plist"));
    let log_path = launchd_log_path(&home, log_name);
    std::fs::create_dir_all(&plist_dir)?;
    std::fs::create_dir_all(log_path.parent().context("log path has no parent")?)?;

    let secrets_path = home.join(".home-still").join("secrets.env");
    let secrets = match std::fs::read_to_string(&secrets_path) {
        Ok(contents) => parse_secrets_env(&contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", secrets_path.display())),
    };

    let plist = render_launchd_plist(label, program_args, &secrets, &log_path);
    write_private_file(&plist_path, &plist)?;
    Ok((plist_path, log_path))
}

// ── Service Installation ───────────────────────────────────────

async fn install_service(
    service_type: &str,
    port: u16,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (service_type, port, reporter);
        anyhow::bail!("--install is only supported on Linux (systemd) and macOS (launchd)");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let hs_bin = std::env::current_exe().context("Cannot find hs binary path")?;
        #[cfg(target_os = "macos")]
        let hs_path = hs_bin.display();

        #[cfg(target_os = "linux")]
        {
            let user = current_user()?;
            let service_name = format!("hs-serve-{service_type}");
            let unit_path = format!("/etc/systemd/system/{service_name}.service");

            let home_dir = dirs::home_dir().context("Cannot find home directory")?;
            let fastembed_cache = hs_bin
                .parent()
                .unwrap_or(home_dir.as_path())
                .join(".fastembed_cache");

            let secrets_path = home_dir.join(".home-still").join("secrets.env");
            let unit = render_system_unit(
                service_type,
                port,
                &user,
                &home_dir,
                &hs_bin,
                &fastembed_cache,
                secrets_path.exists().then_some(secrets_path.as_path()),
            );

            reporter.status("Install", &format!("writing {unit_path}"));
            sudo_write_file(&unit_path, &unit).await?;

            reporter.status("Enable", &format!("{service_name}.service"));
            let status = tokio::process::Command::new("sudo")
                .args(["systemctl", "daemon-reload"])
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("systemctl daemon-reload failed");
            }

            run_checked(
                tokio::process::Command::new("sudo").args(["systemctl", "enable", &service_name]),
                &format!("sudo systemctl enable {service_name}"),
            )
            .await?;
            // `enable --now` leaves an instance that is already running on
            // the old unit; a re-install must pick up the rewritten one.
            run_checked(
                tokio::process::Command::new("sudo").args(["systemctl", "restart", &service_name]),
                &format!("sudo systemctl restart {service_name}"),
            )
            .await?;

            reporter.finish(&format!(
                "Installed and started {service_name}\n\
             View logs: journalctl -u {service_name} -f\n\
             Stop:      sudo systemctl stop {service_name}\n\
             Disable:   sudo systemctl disable {service_name}"
            ));
        }

        #[cfg(target_os = "macos")]
        {
            let label = format!("com.home-still.{service_type}");
            let program_args = vec![
                hs_path.to_string(),
                "serve".to_string(),
                service_type.to_string(),
                "--port".to_string(),
                port.to_string(),
            ];
            let (plist_path, log_path) = write_launchd_agent(&label, service_type, &program_args)?;
            reporter.status("Install", &format!("{}", plist_path.display()));

            reporter.status("Load", &label);
            // Unload first in case it's already loaded (ignore errors)
            let _ = tokio::process::Command::new("launchctl")
                .args(["unload", &plist_path.to_string_lossy()])
                .status()
                .await;

            let status = tokio::process::Command::new("launchctl")
                .args(["load", &plist_path.to_string_lossy()])
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("launchctl load failed");
            }

            reporter.finish(&format!(
                "Installed and started {label}\n\
             View logs: tail -f {}\n\
             Stop:      launchctl unload {}\n\
             Remove:    rm {}",
                log_path.display(),
                plist_path.display(),
                plist_path.display()
            ));
        }

        Ok(())
    } // cfg(any(linux, macos))
}

async fn uninstall_service(service_type: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (service_type, reporter);
        anyhow::bail!("--uninstall is only supported on Linux and macOS");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        {
            let service_name = format!("hs-serve-{service_type}");
            let unit_path = format!("/etc/systemd/system/{service_name}.service");
            if !std::path::Path::new(&unit_path).exists() {
                reporter.finish(&format!("{service_name} is not installed"));
                return Ok(());
            }

            reporter.status("Stop", &service_name);
            run_checked(
                tokio::process::Command::new("sudo").args([
                    "systemctl",
                    "disable",
                    "--now",
                    &service_name,
                ]),
                &format!("sudo systemctl disable --now {service_name}"),
            )
            .await?;
            run_checked(
                tokio::process::Command::new("sudo").args(["rm", "-f", &unit_path]),
                &format!("sudo rm -f {unit_path}"),
            )
            .await?;
            run_checked(
                tokio::process::Command::new("sudo").args(["systemctl", "daemon-reload"]),
                "sudo systemctl daemon-reload",
            )
            .await?;

            reporter.finish(&format!("Removed {service_name}"));
        }

        #[cfg(target_os = "macos")]
        {
            let label = format!("com.home-still.{service_type}");
            let plist_path = hs_common::home_dir()?
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            remove_launch_agent(&label, &plist_path, reporter).await?;
        }

        Ok(())
    }
}

/// Check if a system service is already running for this service type.
/// Prevents conflicts when running `hs serve` in foreground.
fn check_system_service_conflict(service_type: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let service_name = format!("hs-serve-{service_type}");
        // If we ARE the systemd service (INVOCATION_ID is set), don't block ourselves.
        if std::env::var("INVOCATION_ID").is_err() {
            if let Ok(output) = std::process::Command::new("systemctl")
                .args(["is-active", &service_name])
                .output()
            {
                let status = String::from_utf8_lossy(&output.stdout);
                if status.trim() == "active" {
                    anyhow::bail!(
                        "{service_name} is already running via systemd.\n\
                         Stop it first:  sudo systemctl stop {service_name}\n\
                         Or uninstall:   hs serve {service_type} --uninstall"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let label = format!("com.home-still.{service_type}");
        if let Ok(output) = std::process::Command::new("launchctl")
            .args(["list"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // launchctl list format: "PID\tStatus\tLabel". Match the label
            // column exactly — not as a substring — so a service label that
            // is a prefix of another (e.g. `com.home-still.scribe` vs a
            // longer `com.home-still.scribe-*`) can't false-positive and make
            // the wrapper refuse to start.
            let my_pid = std::process::id().to_string();
            for line in stdout.lines() {
                let mut fields = line.split('\t');
                let pid_field = fields.next().unwrap_or("-");
                let _status = fields.next();
                let label_field = fields.next().unwrap_or("");
                if label_field != label {
                    continue;
                }
                if pid_field == "-" || pid_field == my_pid {
                    // Not running, or we are the service — no conflict
                    continue;
                }
                anyhow::bail!(
                    "{label} is already running via launchd (PID {pid_field}).\n\
                     Stop it first:  launchctl unload ~/Library/LaunchAgents/{label}.plist\n\
                     Or uninstall:   hs serve {service_type} --uninstall"
                );
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = service_type;

    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────

pub(crate) fn find_mcp_binary() -> Result<Option<PathBuf>> {
    // Check ~/.local/bin (install script location)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-mcp");
        if path.exists() {
            return Ok(Some(path));
        }
    }
    // Check next to the current binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-mcp");
            if path.exists() {
                return Ok(Some(path));
            }
        }
    }
    // Check cargo target dirs (dev builds)
    let project = hs_common::resolve_project_dir()?;
    for profile in ["release", "debug"] {
        let path = project.join("target").join(profile).join("hs-mcp");
        if path.exists() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn restart_policy(unit: &str) -> (String, u32) {
        let value = |key: &str| {
            unit.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("unit has no {key}"))
                .trim()
                .to_string()
        };
        (
            value("Restart="),
            value("RestartSec=").parse().expect("RestartSec seconds"),
        )
    }

    /// A process that exits 1 (the watch-events "event stream ended" error)
    /// must be restarted, and not in a hot loop.
    fn assert_restarts_on_exit_1(unit: &str) {
        let (restart, sec) = restart_policy(unit);
        assert!(
            matches!(restart.as_str(), "always" | "on-failure"),
            "Restart={restart} does not restart on exit status 1"
        );
        assert!(sec >= 1, "RestartSec={sec} allows a hot restart loop");
    }

    #[test]
    fn resolve_user_has_no_default() {
        assert!(resolve_user(None, || None).is_err());
        assert!(resolve_user(Some("  ".into()), || None).is_err());
        assert_eq!(
            resolve_user(None, || Some("alice\n".into())).unwrap(),
            "alice"
        );
        assert_eq!(
            resolve_user(Some("bob".into()), || panic!("id must not run")).unwrap(),
            "bob"
        );
        // Anything that could add a unit-file line is refused.
        assert!(resolve_user(Some("bob\nExecStartPre=/bin/sh".into()), || None).is_err());
    }

    #[test]
    fn system_unit_names_the_given_user_and_restarts_on_failure() {
        let unit = render_system_unit(
            "scribe",
            7433,
            "alice",
            Path::new("/home/user"),
            Path::new("/home/user/.local/bin/hs"),
            Path::new("/home/user/.local/bin/.fastembed_cache"),
            Some(Path::new("/home/user/.home-still/secrets.env")),
        );
        assert!(unit.lines().any(|l| l == "User=alice"));
        assert!(unit.contains("ExecStart=/home/user/.local/bin/hs serve scribe --port 7433"));
        assert_restarts_on_exit_1(&unit);
    }

    #[test]
    fn user_unit_for_watch_daemons_restarts_on_failure() {
        let unit = render_user_unit(
            "watch",
            Path::new("/home/user"),
            Path::new("/home/user/.local/bin/hs"),
            &["scribe", "watch-events"],
            None,
        );
        assert!(unit.contains("ExecStart=/home/user/.local/bin/hs scribe watch-events"));
        assert_restarts_on_exit_1(&unit);
    }

    /// Minimal inverse of `xml_escape` for the round-trip check.
    fn xml_unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn plist_escapes_secret_values_and_cannot_be_injected() {
        let nasty = "p&ss<w>\"o'rd\n</string><key>Evil</key><string>x";
        let plist = render_launchd_plist(
            "com.home-still.scribe",
            &["/home/user/.local/bin/hs".into(), "a<b".into()],
            &[("TOKEN".into(), nasty.into())],
            Path::new("/home/user/Library/Logs/home-still/hs-scribe.log"),
        );

        // Structure is intact: nothing from the value became markup.
        assert!(!plist.contains("<key>Evil</key>"));
        assert_eq!(plist.matches("<key>").count(), 10);
        // The text between the TOKEN key and its closing tag round-trips.
        let after = plist
            .split("<key>TOKEN</key>\n        <string>")
            .nth(1)
            .unwrap();
        let escaped = after.split("</string>").next().unwrap();
        assert!(!escaped.contains('<') && !escaped.contains('>'));
        assert!(escaped
            .replace("&amp;", "")
            .replace("&lt;", "")
            .replace("&gt;", "")
            .replace("&quot;", "")
            .replace("&apos;", "")
            .find('&')
            .is_none());
        assert_eq!(xml_unescape(escaped), nasty);
    }

    #[test]
    fn plist_restarts_and_logs_to_user_log_dir() {
        let log = launchd_log_path(Path::new("/Users/user"), "scribe");
        assert_eq!(
            log,
            Path::new("/Users/user/Library/Logs/home-still/hs-scribe.log")
        );
        let plist = render_launchd_plist("com.home-still.scribe", &["hs".into()], &[], &log);
        assert!(plist.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(!plist.contains("/tmp/"));
    }

    #[test]
    fn secrets_env_parsing_strips_quotes_and_comments() {
        assert_eq!(
            parse_secrets_env("# c\n\nA=\"x y\"\nB='z'\nC=a=b\n"),
            vec![
                ("A".to_string(), "x y".to_string()),
                ("B".to_string(), "z".to_string()),
                ("C".to_string(), "a=b".to_string()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn written_plist_is_mode_0600_and_replaces_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("com.home-still.scribe.plist");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_file(&path, "secret").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No staging file left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
