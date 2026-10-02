//! `hs mcp` subcommand — install/uninstall MCP server config for Claude & OpenCode clients.

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

/// Atomically replace `path` with `value`: same-directory unique temp file
/// (create_new), fsync, the existing file's permission mode preserved (a new
/// file is 0600: the stdio entry embeds `secrets.env` values), rename over
/// the target. The temp file is removed on every error path.
fn write_config(path: &PathBuf, value: &serde_json::Value) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid config path {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    let text = serde_json::to_string_pretty(value)? + "\n";

    let mut tmp = tempfile::Builder::new()
        .prefix(".mcp-config-")
        .tempfile_in(parent) // create_new, 0600, removed on drop
        .with_context(|| format!("Failed to create temp file in {}", parent.display()))?;
    {
        use std::io::Write as _;
        tmp.write_all(text.as_bytes())
            .with_context(|| format!("Failed to write {}", path.display()))?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;
    }
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(tmp.path(), meta.permissions())
            .with_context(|| format!("Failed to preserve permissions of {}", path.display()))?;
    }
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("Failed to replace {}", path.display()))?;
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
        let bin = match super::serve_cmd::find_mcp_binary()? {
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

/// Install `hs-mcp` from the release tagged like the running `hs`, through
/// the shared installer (checksum + version verified).
async fn download_mcp_binary(reporter: &Arc<dyn Reporter>) -> Result<PathBuf> {
    let version = env!("HS_VERSION");
    // Normalize version to tag format (e.g. "0.0.1-rc.173" → "v0.0.1-rc.173")
    let tag = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    let target = crate::installer::detect_target()?;
    let installer = crate::installer::Installer::new(crate::installer::DEFAULT_API_BASE, target)?;
    let build_hint = format!("Try: HS_RELEASE_TAG={tag} cargo build --release -p hs-mcp");

    let release = installer
        .release_by_tag(&tag)
        .await
        .with_context(|| format!("Could not find release {tag} on GitHub. {build_hint}"))?;
    let prepared = installer
        .prepare_required(&release, "hs-mcp")
        .await
        .with_context(|| build_hint.clone())?;

    // Install next to the running hs binary, falling back to ~/.local/bin
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/bin"));
    let install_path = install_dir.join("hs-mcp");
    installer
        .install(&prepared, &install_path, reporter)
        .await?;

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_config_preserves_mode_roundtrips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let value = serde_json::json!({"mcpServers": {"home-still": {"command": "/x/hs-mcp"}}});
        write_config(&path, &value).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(read_config(&path).unwrap(), value);
        assert_eq!(entries(dir.path()), vec!["claude.json"]);
    }

    #[test]
    fn write_config_creates_new_file_private_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/config.json");
        write_config(&path, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn failed_write_leaves_old_config_intact_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "OLD-CONTENT").unwrap();
        // Directory not writable: temp creation fails before the target is touched.
        // (Skipped as root, where mode bits do not restrict writes.)
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let res = write_config(&path, &serde_json::json!({"new": true}));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "OLD-CONTENT");
        assert_eq!(entries(dir.path()), vec!["c.json"]);
    }

    #[test]
    fn failed_rename_removes_temp_and_keeps_old_target() {
        // Target is a non-empty directory: rename over it fails after the temp is written.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "x").unwrap();
        assert!(write_config(&path, &serde_json::json!({})).is_err());
        assert_eq!(entries(dir.path()), vec!["c.json"]);
        assert!(path.join("keep").exists());
    }
}
