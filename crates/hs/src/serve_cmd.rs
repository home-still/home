use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::auth::client::AuthenticatedClient;
use hs_common::reporter::Reporter;

const DEFAULT_SCRIBE_PORT: u16 = 7433;
const DEFAULT_DISTILL_PORT: u16 = 7434;
const DEFAULT_MCP_PORT: u16 = 7445;

const HEARTBEAT_INTERVAL_SECS: u64 = 30;

#[derive(Subcommand, Debug)]
pub enum ServeCmd {
    /// Run a scribe server (auto-init, foreground, registers with gateway)
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
    /// Run a distill server (auto-init, foreground, registers with gateway)
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
    /// Run an MCP server (foreground, registers with gateway)
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
    reporter.status("Serve", &format!("scribe on port {port}"));

    // Auto-init (idempotent — skips already-present steps)
    reporter.status("Init", "checking scribe prerequisites");
    super::scribe_cmd::ensure_init(false).await?;

    // Register with gateway (best-effort); auto-deregisters on drop
    let my_url = format!("http://{}:{port}", local_ip_hint());
    let _reg = RegistryGuard::try_register("scribe", &my_url, reporter).await;

    // Start server (foreground — blocks until shutdown)
    reporter.status("Start", "starting scribe server");
    let result = super::scribe_cmd::start_server_foreground(port, reporter).await;
    // _reg drops here → heartbeat aborted, deregister sent

    reporter.finish("scribe server stopped");
    result
}

// ── Distill ────────────────────────────────────────────────────

async fn serve_distill(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", &format!("distill on port {port}"));

    // Auto-init (idempotent)
    reporter.status("Init", "checking distill prerequisites");
    super::distill_cmd::ensure_init(reporter).await?;

    // Register with gateway; auto-deregisters on drop
    let my_url = format!("http://{}:{port}", local_ip_hint());
    let _reg = RegistryGuard::try_register("distill", &my_url, reporter).await;

    // Start server (foreground — blocks until shutdown)
    reporter.status("Start", "starting distill server");
    let result = super::distill_cmd::start_server_foreground(port, reporter).await;

    reporter.finish("distill server stopped");
    result
}

// ── MCP ────────────────────────────────────────────────────────

async fn serve_mcp(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    reporter.status("Serve", &format!("mcp on port {port}"));

    let binary = find_mcp_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "hs-mcp binary not found. Build with:\n  \
             cargo build --release -p hs-mcp"
        )
    })?;

    let addr = format!("0.0.0.0:{port}");

    // Register with gateway; auto-deregisters on drop
    let my_url = format!("http://{}:{port}", local_ip_hint());
    let _reg = RegistryGuard::try_register("mcp", &my_url, reporter).await;

    reporter.status("Start", &format!("hs-mcp --serve {addr}"));

    // Spawn child and forward SIGTERM for graceful shutdown
    let mut child = tokio::process::Command::new(&binary)
        .args(["--serve", &addr])
        .spawn()
        .context("Failed to start hs-mcp")?;

    // Wait for either child exit or Ctrl+C
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = tokio::signal::ctrl_c() => {
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

    if !status.success() {
        anyhow::bail!("hs-mcp exited with {status}");
    }

    reporter.finish("mcp server stopped");
    Ok(())
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
        let ip = local_ip_hint();
        let hs_bin = std::env::current_exe().context("Cannot find hs binary path")?;
        let hs_path = hs_bin.display();

        #[cfg(target_os = "linux")]
        {
            let user = std::env::var("USER").unwrap_or_else(|_| "ladvien".into());
            let service_name = format!("hs-serve-{service_type}");
            let unit_path = format!("/etc/systemd/system/{service_name}.service");

            let home_dir = dirs::home_dir().unwrap_or_default();
            let fastembed_cache = hs_bin
                .parent()
                .unwrap_or(home_dir.as_path())
                .join(".fastembed_cache");

            let unit = format!(
                r#"[Unit]
Description=Home-Still {service_type} server
After=network.target

[Service]
Type=simple
User={user}
WorkingDirectory={home}
Environment=HS_ADVERTISE_IP={ip}
Environment=FASTEMBED_CACHE_PATH={cache}
ExecStart={hs_path} serve {service_type} --port {port}
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
"#,
                home = home_dir.display(),
                cache = fastembed_cache.display(),
            );

            reporter.status("Install", &format!("writing {unit_path}"));

            // Write unit file (needs sudo)
            let tmp = format!("/tmp/{service_name}.service");
            std::fs::write(&tmp, &unit).context("Failed to write temp unit file")?;

            let status = tokio::process::Command::new("sudo")
                .args(["cp", &tmp, &unit_path])
                .status()
                .await
                .context("sudo cp failed")?;
            if !status.success() {
                anyhow::bail!("Failed to install systemd unit (sudo cp)");
            }
            let _ = std::fs::remove_file(&tmp);

            reporter.status("Enable", &format!("{service_name}.service"));
            let status = tokio::process::Command::new("sudo")
                .args(["systemctl", "daemon-reload"])
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("systemctl daemon-reload failed");
            }

            let status = tokio::process::Command::new("sudo")
                .args(["systemctl", "enable", "--now", &service_name])
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("systemctl enable --now failed");
            }

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
            let plist_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
                .join("Library/LaunchAgents");
            let plist_path = plist_dir.join(format!("{label}.plist"));

            std::fs::create_dir_all(&plist_dir)?;

            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://schemas.apple.com/dtds/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{hs_path}</string>
        <string>serve</string>
        <string>{service_type}</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HS_ADVERTISE_IP</key>
        <string>{ip}</string>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/hs-{service_type}.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/hs-{service_type}.log</string>
</dict>
</plist>
"#
            );

            reporter.status("Install", &format!("{}", plist_path.display()));
            std::fs::write(&plist_path, &plist)?;

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
             View logs: tail -f /tmp/hs-{service_type}.log\n\
             Stop:      launchctl unload {}\n\
             Remove:    rm {}",
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

            reporter.status("Stop", &service_name);
            let _ = tokio::process::Command::new("sudo")
                .args(["systemctl", "stop", &service_name])
                .status()
                .await;
            let _ = tokio::process::Command::new("sudo")
                .args(["systemctl", "disable", &service_name])
                .status()
                .await;
            let _ = tokio::process::Command::new("sudo")
                .args(["rm", "-f", &unit_path])
                .status()
                .await;
            let _ = tokio::process::Command::new("sudo")
                .args(["systemctl", "daemon-reload"])
                .status()
                .await;

            reporter.finish(&format!("Removed {service_name}"));
        }

        #[cfg(target_os = "macos")]
        {
            let label = format!("com.home-still.{service_type}");
            let plist_path = dirs::home_dir()
                .unwrap_or_default()
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));

            reporter.status("Unload", &label);
            let _ = tokio::process::Command::new("launchctl")
                .args(["unload", &plist_path.to_string_lossy()])
                .status()
                .await;
            let _ = std::fs::remove_file(&plist_path);

            reporter.finish(&format!("Removed {label}"));
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
            // launchctl list format: "PID\tStatus\tLabel"
            // If PID is "-", the service is registered but not running.
            // If PID matches our own, we ARE the launchd child — don't block ourselves.
            let my_pid = std::process::id().to_string();
            for line in stdout.lines() {
                if !line.contains(&label) {
                    continue;
                }
                let pid_field = line.split('\t').next().unwrap_or("-");
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

// ── Registry Integration ───────────────────────────────────────

/// RAII guard for gateway registration. Aborts heartbeat and sends deregister on drop.
struct RegistryGuard {
    service_type: String,
    url: String,
    gateway_url: String,
    auth: Arc<AuthenticatedClient>,
    http: reqwest::Client,
    heartbeat_handle: tokio::task::JoinHandle<()>,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.heartbeat_handle.abort();

        // Best-effort sync deregister — spawn a task since Drop can't be async
        let http = self.http.clone();
        let gateway_url = self.gateway_url.clone();
        let auth = Arc::clone(&self.auth);
        let body = serde_json::json!({
            "service_type": self.service_type,
            "url": self.url,
        });
        tokio::spawn(async move {
            if let Ok(token) = auth.get_access_token().await {
                let _ = http
                    .delete(format!("{gateway_url}/registry/deregister"))
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await;
            }
        });
    }
}

impl RegistryGuard {
    /// Try to register with the gateway. Returns None if not enrolled.
    async fn try_register(
        service_type: &str,
        url: &str,
        reporter: &Arc<dyn Reporter>,
    ) -> Option<Self> {
        let auth = match AuthenticatedClient::from_default_path() {
            Ok(a) => Arc::new(a),
            Err(_) => {
                reporter.warn("Not enrolled with gateway — running in local-only mode");
                return None;
            }
        };

        let gateway_url = auth.gateway_url().to_string();
        let token = match auth.get_access_token().await {
            Ok(t) => t,
            Err(e) => {
                reporter.warn(&format!("Could not get gateway token: {e}"));
                return None;
            }
        };

        // Shared HTTP client for register, heartbeats, and deregister
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // Register
        let body = serde_json::json!({
            "service_type": service_type,
            "url": url,
            "metadata": {}
        });

        let resp = http
            .post(format!("{gateway_url}/registry/register"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                reporter.status(
                    "Registry",
                    &format!("registered as {service_type} at {url}"),
                );
            }
            Ok(r) => {
                reporter.warn(&format!("Registry registration failed ({})", r.status()));
                return None;
            }
            Err(e) => {
                reporter.warn(&format!("Gateway unreachable: {e}"));
                return None;
            }
        }

        // Start heartbeat loop with shared client and error logging
        let hb_auth = Arc::clone(&auth);
        let hb_http = http.clone();
        let hb_type = service_type.to_string();
        let hb_url = url.to_string();
        let hb_gateway = gateway_url.clone();
        let heartbeat_handle = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
            let mut consecutive_failures = 0u32;
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                let token = match hb_auth.get_access_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures <= 3 {
                            tracing::warn!("Heartbeat token refresh failed: {e}");
                        }
                        continue;
                    }
                };
                let body = serde_json::json!({
                    "service_type": hb_type,
                    "url": hb_url,
                });
                match hb_http
                    .post(format!("{hb_gateway}/registry/heartbeat"))
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        consecutive_failures = 0;
                    }
                    Ok(r) => {
                        consecutive_failures += 1;
                        if consecutive_failures <= 3 {
                            tracing::warn!("Heartbeat rejected: {}", r.status());
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures <= 3 {
                            tracing::warn!("Heartbeat failed: {e}");
                        }
                    }
                }
            }
        });

        Some(Self {
            service_type: service_type.to_string(),
            url: url.to_string(),
            gateway_url,
            auth,
            http,
            heartbeat_handle,
        })
    }
}

// ── Helpers ────────────────────────────────────────────────────

/// Best-effort local IP detection for registration URL.
/// Checks: HS_ADVERTISE_IP env var → platform-specific detection → 127.0.0.1.
fn local_ip_hint() -> String {
    // Allow explicit override via environment variable
    if let Ok(ip) = std::env::var("HS_ADVERTISE_IP") {
        if !ip.is_empty() {
            return ip;
        }
    }

    // Linux: `ip route get 1.1.1.1` — most reliable, returns the outbound source IP
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("ip")
            .args(["route", "get", "1.1.1.1"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Output: "1.1.1.1 via 192.168.1.1 dev enp6s0 src 192.168.1.110 uid 1000"
            if let Some(pos) = stdout.find("src ") {
                let after_src = &stdout[pos + 4..];
                if let Some(ip) = after_src.split_whitespace().next() {
                    if !ip.starts_with("127.") {
                        return ip.to_string();
                    }
                }
            }
        }
        // Fallback: hostname -I
        if let Ok(output) = std::process::Command::new("hostname").arg("-I").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(ip) = stdout.split_whitespace().next() {
                if !ip.starts_with("127.") {
                    return ip.to_string();
                }
            }
        }
    }

    // macOS: `route get default` then `ipconfig getifaddr <iface>`
    #[cfg(target_os = "macos")]
    {
        // Try en0 first (most common), then en1
        for iface in &["en0", "en1"] {
            if let Ok(output) = std::process::Command::new("ipconfig")
                .args(["getifaddr", iface])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let ip = stdout.trim();
                if !ip.is_empty() && !ip.starts_with("127.") {
                    return ip.to_string();
                }
            }
        }
    }

    "127.0.0.1".into()
}

pub(crate) fn find_mcp_binary() -> Option<PathBuf> {
    // Check ~/.local/bin (install script location)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-mcp");
        if path.exists() {
            return Some(path);
        }
    }
    // Check next to the current binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-mcp");
            if path.exists() {
                return Some(path);
            }
        }
    }
    // Check cargo target dirs (dev builds)
    let project = hs_common::resolve_project_dir();
    for profile in ["release", "debug"] {
        let path = project.join("target").join(profile).join("hs-mcp");
        if path.exists() {
            return Some(path);
        }
    }
    None
}
