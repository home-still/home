//! `hs mcp` subcommand — install/uninstall MCP server config for Claude clients.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use hs_common::reporter::Reporter;

#[derive(Clone, Debug, ValueEnum)]
pub enum McpClient {
    /// Claude Desktop app
    Desktop,
    /// Claude Code CLI / IDE extension
    Code,
    /// Both Desktop and Code
    All,
}

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Install MCP server config into Claude Desktop and/or Claude Code
    Install {
        /// Target client
        #[arg(long, value_enum, default_value = "all")]
        client: McpClient,
        /// Configure remote access via cloud gateway instead of local stdio
        #[arg(long)]
        remote: bool,
        /// Gateway URL for remote mode (reads from cloud config if omitted)
        #[arg(long)]
        gateway_url: Option<String>,
    },
    /// Remove MCP server config from Claude Desktop and/or Claude Code
    Uninstall {
        /// Target client
        #[arg(long, value_enum, default_value = "all")]
        client: McpClient,
    },
}

pub async fn dispatch(cmd: McpCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        McpCmd::Install {
            client,
            remote,
            gateway_url,
        } => cmd_install(client, remote, gateway_url, reporter).await,
        McpCmd::Uninstall { client } => cmd_uninstall(client, reporter).await,
    }
}

// ── Config paths ───────────────────────────────────────────────

fn claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir()?;
        Some(home.join("Library/Application Support/Claude/claude_desktop_config.json"))
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir()?;
        Some(home.join(".config/Claude/claude_desktop_config.json"))
    }

    #[cfg(target_os = "windows")]
    {
        dirs::config_dir().map(|c| c.join("Claude/claude_desktop_config.json"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn claude_code_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude.json"))
}

fn config_paths(client: &McpClient) -> Vec<(&'static str, PathBuf)> {
    let mut paths = Vec::new();
    match client {
        McpClient::Desktop => {
            if let Some(p) = claude_desktop_config_path() {
                paths.push(("Claude Desktop", p));
            }
        }
        McpClient::Code => {
            if let Some(p) = claude_code_config_path() {
                paths.push(("Claude Code", p));
            }
        }
        McpClient::All => {
            if let Some(p) = claude_desktop_config_path() {
                paths.push(("Claude Desktop", p));
            }
            if let Some(p) = claude_code_config_path() {
                paths.push(("Claude Code", p));
            }
        }
    }
    paths
}

// ── JSON helpers ───────────────────────────────────────────────

fn read_config(path: &PathBuf) -> Result<serde_json::Value> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let val: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Invalid JSON in {}", path.display()))?;
        Ok(val)
    } else {
        Ok(serde_json::json!({}))
    }
}

fn write_config(path: &PathBuf, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

// ── Install ────────────────────────────────────────────────────

fn build_stdio_entry(mcp_bin: &Path) -> serde_json::Value {
    serde_json::json!({
        "command": mcp_bin.to_string_lossy(),
    })
}

fn build_remote_entry(gateway_url: &str) -> serde_json::Value {
    let url = format!("{}/mcp", gateway_url.trim_end_matches('/'));
    serde_json::json!({
        "type": "url",
        "url": url,
    })
}

async fn resolve_gateway_url(explicit: Option<String>) -> Result<String> {
    if let Some(url) = explicit {
        return Ok(url);
    }
    // Try to read from cloud credentials
    let cred_path = hs_common::auth::client::CloudCredentials::default_path();
    if cred_path.exists() {
        let creds = hs_common::auth::client::CloudCredentials::load(&cred_path)?;
        return Ok(creds.gateway_url);
    }
    anyhow::bail!(
        "No gateway URL provided and no cloud credentials found.\n\
         Either pass --gateway-url or run `hs cloud enroll` first."
    );
}

async fn cmd_install(
    client: McpClient,
    remote: bool,
    gateway_url: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let entry = if remote {
        let url = resolve_gateway_url(gateway_url).await?;
        reporter.status("Mode", &format!("remote ({})", url));
        build_remote_entry(&url)
    } else {
        let mcp_bin = super::serve_cmd::find_mcp_binary().ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find hs-mcp binary.\n\
                 Install it with: cargo build --release -p hs-mcp\n\
                 Or: curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh"
            )
        })?;
        reporter.status("Mode", &format!("local stdio ({})", mcp_bin.display()));
        build_stdio_entry(&mcp_bin)
    };

    let paths = config_paths(&client);
    if paths.is_empty() {
        anyhow::bail!("No supported Claude config path found for this platform");
    }

    for (name, path) in &paths {
        let mut config = read_config(path)?;

        let servers = config
            .as_object_mut()
            .context("Config is not a JSON object")?
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}));

        servers
            .as_object_mut()
            .context("mcpServers is not a JSON object")?
            .insert("home-still".to_string(), entry.clone());

        write_config(path, &config)?;
        reporter.status("Installed", &format!("{} ({})", name, path.display()));
    }

    reporter.finish("MCP server configured. Restart Claude to pick up the changes.");
    Ok(())
}

// ── Uninstall ──────────────────────────────────────────────────

async fn cmd_uninstall(client: McpClient, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let paths = config_paths(&client);
    if paths.is_empty() {
        anyhow::bail!("No supported Claude config path found for this platform");
    }

    for (name, path) in &paths {
        if !path.exists() {
            reporter.status("Skipped", &format!("{} (no config file)", name));
            continue;
        }

        let mut config = read_config(path)?;

        let removed = config
            .as_object_mut()
            .and_then(|obj| obj.get_mut("mcpServers"))
            .and_then(|servers| servers.as_object_mut())
            .map(|servers| servers.remove("home-still").is_some())
            .unwrap_or(false);

        if removed {
            write_config(path, &config)?;
            reporter.status("Removed", &format!("{} ({})", name, path.display()));
        } else {
            reporter.status("Skipped", &format!("{} (not configured)", name));
        }
    }

    reporter.finish("MCP server config removed.");
    Ok(())
}
