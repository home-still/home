use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::auth::client::AuthenticatedClient;
use hs_common::reporter::Reporter;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum ServerCmd {
    /// List all registered servers with health status
    List,
    /// Manually add a server to the registry
    Add {
        /// Service type (scribe, distill, mcp)
        service_type: String,
        /// Server URL (e.g. http://192.168.1.110:7433)
        url: String,
    },
    /// Remove a server from the registry
    Remove {
        /// Service type (scribe, distill, mcp)
        service_type: String,
        /// Server URL
        url: String,
    },
    /// Disable a server (stop routing work, keep registered)
    Disable {
        /// Service type (scribe, distill, mcp)
        service_type: String,
        /// Server URL
        url: String,
    },
    /// Re-enable a disabled server
    Enable {
        /// Service type (scribe, distill, mcp)
        service_type: String,
        /// Server URL
        url: String,
    },
}

const VALID_SERVICE_TYPES: &[&str] = &["scribe", "distill", "mcp"];

fn validate_args(service_type: &str, url: &str) -> Result<()> {
    if !VALID_SERVICE_TYPES.contains(&service_type) {
        anyhow::bail!(
            "Invalid service type '{}'. Must be one of: {}",
            service_type,
            VALID_SERVICE_TYPES.join(", ")
        );
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("URL must start with http:// or https://");
    }
    Ok(())
}

pub async fn dispatch(cmd: ServerCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        ServerCmd::List => cmd_list(reporter).await,
        ServerCmd::Add {
            service_type,
            url,
        } => {
            validate_args(&service_type, &url)?;
            cmd_add(&service_type, &url, reporter).await
        }
        ServerCmd::Remove {
            service_type,
            url,
        } => {
            validate_args(&service_type, &url)?;
            cmd_remove(&service_type, &url, reporter).await
        }
        ServerCmd::Disable {
            service_type,
            url,
        } => {
            validate_args(&service_type, &url)?;
            cmd_set_enabled(&service_type, &url, false, reporter).await
        }
        ServerCmd::Enable {
            service_type,
            url,
        } => {
            validate_args(&service_type, &url)?;
            cmd_set_enabled(&service_type, &url, true, reporter).await
        }
    }
}

// ── List ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ServicesResponse {
    services: Vec<ServiceInfo>,
}

#[derive(Deserialize)]
struct ServiceInfo {
    service_type: String,
    url: String,
    device_name: String,
    enabled: bool,
    healthy: bool,
    last_heartbeat_secs_ago: u64,
    #[serde(default)]
    metadata: ServiceMetadata,
}

#[derive(Deserialize, Default)]
struct ServiceMetadata {
    compute_device: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
}

async fn cmd_list(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let auth = AuthenticatedClient::from_default_path()
        .context("Not enrolled with gateway. Run `hs cloud enroll` first.")?;

    let token = auth.get_access_token().await?;
    let gateway_url = auth.gateway_url();

    let resp: ServicesResponse = reqwest::Client::new()
        .get(format!("{gateway_url}/registry/services"))
        .bearer_auth(&token)
        .send()
        .await
        .context("Could not reach gateway")?
        .error_for_status()
        .context("Gateway returned error")?
        .json()
        .await?;

    if resp.services.is_empty() {
        reporter.status("Fleet", "no servers registered");
        return Ok(());
    }

    // Print table header
    println!(
        "{:<8} {:<35} {:<10} {:<8} {:<10} HEARTBEAT",
        "TYPE", "URL", "DEVICE", "STATUS", "COMPUTE"
    );

    for svc in &resp.services {
        let status = if !svc.enabled {
            "disabled"
        } else if svc.healthy {
            "ok"
        } else {
            "stale"
        };
        let compute = svc
            .metadata
            .compute_device
            .as_deref()
            .unwrap_or("—");
        let heartbeat = format!("{}s ago", svc.last_heartbeat_secs_ago);

        println!(
            "{:<8} {:<35} {:<10} {:<8} {:<10} {}",
            svc.service_type, svc.url, svc.device_name, status, compute, heartbeat
        );
    }

    Ok(())
}

// ── Add ────────────────────────────────────────────────────────

async fn cmd_add(service_type: &str, url: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let auth = AuthenticatedClient::from_default_path()
        .context("Not enrolled with gateway. Run `hs cloud enroll` first.")?;

    let token = auth.get_access_token().await?;
    let gateway_url = auth.gateway_url().to_string();

    let body = serde_json::json!({
        "service_type": service_type,
        "url": url,
        "metadata": {}
    });

    reqwest::Client::new()
        .post(format!("{gateway_url}/registry/register"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .context("Could not reach gateway")?
        .error_for_status()
        .context("Registration failed")?;

    reporter.status("Added", &format!("{service_type} at {url}"));
    Ok(())
}

// ── Remove ─────────────────────────────────────────────────────

async fn cmd_remove(service_type: &str, url: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let auth = AuthenticatedClient::from_default_path()
        .context("Not enrolled with gateway. Run `hs cloud enroll` first.")?;

    let token = auth.get_access_token().await?;
    let gateway_url = auth.gateway_url().to_string();

    let body = serde_json::json!({
        "service_type": service_type,
        "url": url,
    });

    reqwest::Client::new()
        .delete(format!("{gateway_url}/registry/deregister"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .context("Could not reach gateway")?
        .error_for_status()
        .context("Deregistration failed")?;

    reporter.status("Removed", &format!("{service_type} at {url}"));
    Ok(())
}

// ── Enable / Disable ───────────────────────────────────────────

async fn cmd_set_enabled(
    service_type: &str,
    url: &str,
    enabled: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let auth = AuthenticatedClient::from_default_path()
        .context("Not enrolled with gateway. Run `hs cloud enroll` first.")?;

    let token = auth.get_access_token().await?;
    let gateway_url = auth.gateway_url().to_string();

    let body = serde_json::json!({
        "service_type": service_type,
        "url": url,
        "enabled": enabled,
    });

    reqwest::Client::new()
        .post(format!("{gateway_url}/registry/set-enabled"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .context("Could not reach gateway")?
        .error_for_status()
        .context("Failed to update server")?;

    let action = if enabled { "Enabled" } else { "Disabled" };
    reporter.status(action, &format!("{service_type} at {url}"));
    Ok(())
}
