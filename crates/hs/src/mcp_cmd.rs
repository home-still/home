//! `hs mcp` subcommand — install/uninstall MCP server config for Claude & OpenCode clients.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use hs_common::reporter::Reporter;

const GITHUB_API_RELEASES: &str = "https://api.github.com/repos/home-still/home/releases";

#[derive(Clone, Debug, ValueEnum)]
pub enum McpClient {
    /// Claude Desktop app
    Desktop,
    /// Claude Code CLI / IDE extension
    Code,
    /// OpenCode terminal AI assistant
    Opencode,
    /// All supported clients
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

fn opencode_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".config/opencode/opencode.json"))
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
        McpClient::Opencode => {
            if let Some(p) = opencode_config_path() {
                paths.push(("OpenCode", p));
            }
        }
        McpClient::All => {
            if let Some(p) = claude_desktop_config_path() {
                paths.push(("Claude Desktop", p));
            }
            if let Some(p) = claude_code_config_path() {
                paths.push(("Claude Code", p));
            }
            if let Some(p) = opencode_config_path() {
                paths.push(("OpenCode", p));
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
    let mut entry = serde_json::json!({
        "command": mcp_bin.to_string_lossy(),
    });
    if let Some(env) = secrets_as_json() {
        entry["env"] = env;
    }
    entry
}

/// Load `~/.home-still/secrets.env` and return its KEY=VALUE pairs as a JSON
/// object suitable for dropping into a Claude Desktop / opencode MCP entry's
/// `env` field. Returns `None` if the file is absent or empty.
fn secrets_as_json() -> Option<serde_json::Value> {
    let path = hs_common::secrets::default_path()?;
    let entries = hs_common::secrets::parse_secrets_from_path(&path)
        .ok()
        .flatten()?;
    if entries.is_empty() {
        return None;
    }
    let map: serde_json::Map<String, serde_json::Value> = entries
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    Some(serde_json::Value::Object(map))
}

fn build_remote_entry(gateway_url: &str) -> serde_json::Value {
    let url = format!("{}/mcp", gateway_url.trim_end_matches('/'));
    serde_json::json!({
        "type": "url",
        "url": url,
    })
}

fn build_opencode_stdio_entry(mcp_bin: &Path) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "type": "local",
        "command": [mcp_bin.to_string_lossy()],
        "enabled": true,
    });
    if let Some(env) = secrets_as_json() {
        entry["environment"] = env;
    }
    entry
}

fn build_opencode_remote_entry(gateway_url: &str) -> serde_json::Value {
    let url = format!("{}/mcp", gateway_url.trim_end_matches('/'));
    serde_json::json!({
        "type": "remote",
        "url": url,
        "enabled": true,
    })
}

fn is_opencode(client_name: &str) -> bool {
    client_name == "OpenCode"
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
    let resolved_url = if remote {
        Some(resolve_gateway_url(gateway_url).await?)
    } else {
        None
    };

    let mcp_bin = if !remote {
        let bin = match super::serve_cmd::find_mcp_binary() {
            Some(p) => p,
            None => {
                reporter.status("hs-mcp", "not found locally, downloading from GitHub...");
                download_mcp_binary(reporter).await?
            }
        };
        reporter.status("Mode", &format!("local stdio ({})", bin.display()));
        Some(bin)
    } else {
        reporter.status(
            "Mode",
            &format!("remote ({})", resolved_url.as_deref().unwrap()),
        );
        None
    };

    let paths = config_paths(&client);
    if paths.is_empty() {
        anyhow::bail!("No supported config path found for this platform");
    }

    for (name, path) in &paths {
        let mut config = read_config(path)?;

        if is_opencode(name) {
            let entry = if let Some(ref url) = resolved_url {
                build_opencode_remote_entry(url)
            } else {
                build_opencode_stdio_entry(mcp_bin.as_deref().unwrap())
            };

            let servers = config
                .as_object_mut()
                .context("Config is not a JSON object")?
                .entry("mcp")
                .or_insert_with(|| serde_json::json!({}));

            servers
                .as_object_mut()
                .context("mcp is not a JSON object")?
                .insert("home-still".to_string(), entry);
        } else {
            let entry = if let Some(ref url) = resolved_url {
                build_remote_entry(url)
            } else {
                build_stdio_entry(mcp_bin.as_deref().unwrap())
            };

            let servers = config
                .as_object_mut()
                .context("Config is not a JSON object")?
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}));

            servers
                .as_object_mut()
                .context("mcpServers is not a JSON object")?
                .insert("home-still".to_string(), entry);
        }

        write_config(path, &config)?;
        reporter.status("Installed", &format!("{} ({})", name, path.display()));
    }

    reporter.finish("MCP server configured. Restart your client to pick up the changes.");
    Ok(())
}

// ── Binary download ───────────────────────────────────────────

fn detect_target() -> Result<&'static str> {
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        return Ok("x86_64-apple-darwin");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        return Ok("aarch64-apple-darwin");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        return Ok("x86_64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        return Ok("aarch64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        return Ok("x86_64-pc-windows-msvc");
    }
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!("Unsupported platform"))
}

/// Download `hs-mcp` from the same release tag as the running `hs` binary.
async fn download_mcp_binary(reporter: &Arc<dyn Reporter>) -> Result<PathBuf> {
    let target = detect_target()?;
    let version = env!("HS_VERSION");
    // Normalize version to tag format (e.g. "0.0.1-rc.173" → "v0.0.1-rc.173")
    let tag = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };

    // Fetch the release matching the current hs version
    let http = reqwest::Client::builder()
        .user_agent(format!("hs/{version}"))
        .build()?;

    let mut req = http.get(format!("{GITHUB_API_RELEASES}/tags/{tag}"));
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }

    let resp = req.send().await.context("Failed to reach GitHub API")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "Could not find release {tag} on GitHub ({}). \
             Try: cargo build --release -p hs-mcp",
            resp.status()
        );
    }

    #[derive(serde::Deserialize)]
    struct Release {
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }

    let release: Release = resp.json().await.context("Invalid release JSON")?;
    let archive_name = format!("hs-mcp-{tag}-{target}.tar.gz");

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| anyhow::anyhow!("No hs-mcp asset for {target} in release {tag}"))?;

    reporter.status("Downloading", &archive_name);

    let resp = reqwest::get(&asset.browser_download_url)
        .await
        .context("Download failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed ({})", resp.status());
    }

    let bytes = resp.bytes().await.context("Failed to read download")?;

    // Extract hs-mcp from tar.gz
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);

    let mut binary_data: Option<Vec<u8>> = None;
    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry?;
        let path = entry.path()?;
        let file_name = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        if file_name == "hs-mcp" {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            binary_data = Some(data);
            break;
        }
    }

    let binary_data = binary_data.ok_or_else(|| anyhow::anyhow!("hs-mcp not found in archive"))?;

    // Install next to the running hs binary, falling back to ~/.local/bin
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/bin"));

    std::fs::create_dir_all(&install_dir)?;

    let install_path = install_dir.join("hs-mcp");
    let tmp_path = install_dir.join(".hs-mcp.install.tmp");

    std::fs::write(&tmp_path, &binary_data)
        .with_context(|| format!("Failed to write to {}", tmp_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::rename(&tmp_path, &install_path)
        .with_context(|| format!("Failed to install hs-mcp to {}", install_path.display()))?;

    reporter.status("Installed", &format!("hs-mcp → {}", install_path.display()));
    Ok(install_path)
}

// ── Uninstall ──────────────────────────────────────────────────

async fn cmd_uninstall(client: McpClient, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let paths = config_paths(&client);
    if paths.is_empty() {
        anyhow::bail!("No supported config path found for this platform");
    }

    for (name, path) in &paths {
        if !path.exists() {
            reporter.status("Skipped", &format!("{} (no config file)", name));
            continue;
        }

        let mut config = read_config(path)?;

        let key = if is_opencode(name) {
            "mcp"
        } else {
            "mcpServers"
        };

        let removed = config
            .as_object_mut()
            .and_then(|obj| obj.get_mut(key))
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
