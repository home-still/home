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
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_SCRIBE_PORT)]
        port: u16,
    },
    /// Run a distill server (auto-init, foreground, registers with gateway)
    Distill {
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_DISTILL_PORT)]
        port: u16,
    },
    /// Run an MCP server (foreground, registers with gateway)
    Mcp {
        /// Port to listen on
        #[arg(long, default_value_t = DEFAULT_MCP_PORT)]
        port: u16,
    },
}

pub async fn dispatch(cmd: ServeCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        ServeCmd::Scribe { port } => serve_scribe(port, reporter).await,
        ServeCmd::Distill { port } => serve_distill(port, reporter).await,
        ServeCmd::Mcp { port } => serve_mcp(port, reporter).await,
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
                reporter.status("Registry", &format!("registered as {service_type} at {url}"));
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

    // Linux: hostname -I
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("hostname").arg("-I").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(ip) = stdout.split_whitespace().next() {
                if !ip.starts_with("127.") {
                    return ip.to_string();
                }
            }
        }
    }

    // macOS: ipconfig getifaddr en0
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("ipconfig")
            .args(["getifaddr", "en0"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let ip = stdout.trim();
            if !ip.is_empty() && !ip.starts_with("127.") {
                return ip.to_string();
            }
        }
    }

    "127.0.0.1".into()
}

fn find_mcp_binary() -> Option<PathBuf> {
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
