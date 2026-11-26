//! `hs mcp` subcommand — install/uninstall the two MCP servers in Claude and
//! OpenCode client configuration: `home-still` (hs-mcp, the tools) and
//! `myelin` (the memory server, where agents search and remember).

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
    /// Write the `home-still` and `myelin` servers into your clients' configuration
    Install {
        /// Target client
        #[arg(long, value_enum, default_value = "all")]
        client: McpClient,
        /// Reach both servers through the cloud gateway instead of on this machine
        #[arg(long)]
        remote: bool,
        /// Gateway URL for remote mode (reads from cloud config if omitted)
        #[arg(long)]
        gateway_url: Option<String>,
    },
    /// Remove the `home-still` and `myelin` servers from your clients' configuration
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

/// The entry name of the tools server, hs-mcp.
const TOOLS_SERVER: &str = "home-still";

/// The entry name of the memory server, myelin: where an agent searches the
/// corpus and keeps what it was asked to remember.
const MEMORY_SERVER: &str = "myelin";

/// Where the memory server listens for agents on its own machine.
const LOCAL_MEMORY_URL: &str = "http://127.0.0.1:7463/mcp";

/// The gateway path of the tools server.
const GATEWAY_TOOLS_PATH: &str = "/mcp";

/// The gateway path of the memory server.
const GATEWAY_MEMORY_PATH: &str = "/memory/mcp";

/// A client whose configuration file `hs mcp install` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Desktop,
    Code,
    OpenCode,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Target::Desktop => "Claude Desktop",
            Target::Code => "Claude Code",
            Target::OpenCode => "OpenCode",
        }
    }

    fn config_path(self) -> Option<PathBuf> {
        match self {
            Target::Desktop => claude_desktop_config_path(),
            Target::Code => claude_code_config_path(),
            Target::OpenCode => opencode_config_path(),
        }
    }

    /// The key of the table that holds the servers in this client's file.
    fn servers_key(self) -> &'static str {
        match self {
            Target::Desktop | Target::Code => "mcpServers",
            Target::OpenCode => "mcp",
        }
    }

    /// The entry that starts a server as the client's child process.
    fn command_entry(self, bin: &Path, env: Option<&serde_json::Value>) -> serde_json::Value {
        let bin = bin.to_string_lossy();
        let (mut entry, env_key) = match self {
            Target::Desktop | Target::Code => (serde_json::json!({ "command": bin }), "env"),
            Target::OpenCode => (
                serde_json::json!({ "type": "local", "command": [bin], "enabled": true }),
                "environment",
            ),
        };
        if let Some(env) = env {
            entry[env_key] = env.clone();
        }
        entry
    }

    /// The entry that names a server by URL, or `None` for a client whose
    /// file cannot name one. Claude Desktop reads only commands from its
    /// file and is given remote servers in the app, so nothing is written
    /// for it: a URL entry there is an invalid one.
    fn url_entry(self, url: &str) -> Option<serde_json::Value> {
        match self {
            Target::Desktop => None,
            // The shape `claude mcp add --transport http` writes. Claude
            // Code refuses `"type": "url"` as an invalid configuration.
            Target::Code => Some(serde_json::json!({ "type": "http", "url": url })),
            Target::OpenCode => {
                Some(serde_json::json!({ "type": "remote", "url": url, "enabled": true }))
            }
        }
    }
}

/// The clients `--client` names that have a configuration file on this
/// platform, each with its file.
fn targets(client: &McpClient) -> Vec<(Target, PathBuf)> {
    let wanted: &[Target] = match client {
        McpClient::Desktop => &[Target::Desktop],
        McpClient::Code => &[Target::Code],
        McpClient::Opencode => &[Target::OpenCode],
        McpClient::All => &[Target::Desktop, Target::Code, Target::OpenCode],
    };
    wanted
        .iter()
        .filter_map(|target| Some((*target, target.config_path()?)))
        .collect()
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

/// Load `~/.home-still/secrets.env` and return its KEY=VALUE pairs as a JSON
/// object suitable for dropping into a Claude Desktop / opencode MCP entry's
/// `env` field. Returns `None` if the file is absent or empty; an unreadable
/// file is an error (the entry would otherwise be installed without the
/// credentials the MCP server needs).
fn secrets_as_json() -> Result<Option<serde_json::Value>> {
    let Some(path) = hs_common::secrets::default_path() else {
        return Ok(None);
    };
    let Some(entries) = hs_common::secrets::parse_secrets_from_path(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?
    else {
        return Ok(None);
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let map: serde_json::Map<String, serde_json::Value> = entries
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    Ok(Some(serde_json::Value::Object(map)))
}

async fn resolve_gateway_url(explicit: Option<String>) -> Result<String> {
    if let Some(url) = explicit {
        return Ok(url);
    }
    // Try to read from cloud credentials
    let cred_path = hs_common::auth::client::CloudCredentials::default_path()?;
    if cred_path.exists() {
        let creds = hs_common::auth::client::CloudCredentials::load(&cred_path)?;
        return Ok(creds.gateway_url);
    }
    anyhow::bail!(
        "No gateway URL provided and no cloud credentials found.\n\
         Either pass --gateway-url or run `hs cloud enroll` first."
    );
}

/// How a client reaches the two servers.
enum Mode {
    /// hs-mcp runs as the client's child process, with `secrets.env` as its
    /// environment; the memory server is the one on this machine.
    Local {
        mcp_bin: PathBuf,
        env: Option<serde_json::Value>,
    },
    /// Both servers are behind the gateway.
    Remote { gateway_url: String },
}

/// One server as it is to be written for one client.
struct Planned {
    name: &'static str,
    /// The entry, or `None` when this client's file cannot name the server.
    entry: Option<serde_json::Value>,
    /// What the user does instead when it cannot.
    by_hand: String,
}

/// The servers `hs mcp install` writes for `target`, both of them in either
/// mode.
fn plan(target: Target, mode: &Mode) -> Vec<Planned> {
    match mode {
        Mode::Local { mcp_bin, env } => vec![
            Planned {
                name: TOOLS_SERVER,
                entry: Some(target.command_entry(mcp_bin, env.as_ref())),
                by_hand: String::new(),
            },
            Planned {
                name: MEMORY_SERVER,
                entry: target.url_entry(LOCAL_MEMORY_URL),
                by_hand: format!(
                    "the app cannot reach {LOCAL_MEMORY_URL}; add your gateway's \
                     {GATEWAY_MEMORY_PATH} URL as a connector in the app"
                ),
            },
        ],
        Mode::Remote { gateway_url } => {
            let base = gateway_url.trim_end_matches('/');
            [
                (TOOLS_SERVER, GATEWAY_TOOLS_PATH),
                (MEMORY_SERVER, GATEWAY_MEMORY_PATH),
            ]
            .into_iter()
            .map(|(name, path)| {
                let url = format!("{base}{path}");
                Planned {
                    name,
                    entry: target.url_entry(&url),
                    by_hand: format!("add {url} as a connector in the app"),
                }
            })
            .collect()
        }
    }
}

/// Write `planned` into a client's configuration, replacing entries of the
/// same name and leaving every other server alone. Returns the names
/// written, and the servers this client's file cannot name.
fn write_servers(
    config: &mut serde_json::Value,
    target: Target,
    planned: Vec<Planned>,
) -> Result<(Vec<&'static str>, Vec<Planned>)> {
    let key = target.servers_key();
    let servers = config
        .as_object_mut()
        .context("Config is not a JSON object")?
        .entry(key)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .with_context(|| format!("{key} is not a JSON object"))?;

    let mut written = Vec::new();
    let mut by_hand = Vec::new();
    for mut server in planned {
        match server.entry.take() {
            Some(entry) => {
                servers.insert(server.name.to_string(), entry);
                written.push(server.name);
            }
            None => by_hand.push(server),
        }
    }
    Ok((written, by_hand))
}

/// Remove this project's servers from a client's configuration, returning
/// the names that were there.
fn remove_servers(config: &mut serde_json::Value, target: Target) -> Vec<&'static str> {
    let Some(servers) = config
        .as_object_mut()
        .and_then(|obj| obj.get_mut(target.servers_key()))
        .and_then(|servers| servers.as_object_mut())
    else {
        return Vec::new();
    };
    [TOOLS_SERVER, MEMORY_SERVER]
        .into_iter()
        .filter(|name| servers.remove(*name).is_some())
        .collect()
}

async fn cmd_install(
    client: McpClient,
    remote: bool,
    gateway_url: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let targets = targets(&client);
    if targets.is_empty() {
        anyhow::bail!("No supported config path found for this platform");
    }

    let mode = if remote {
        let gateway_url = resolve_gateway_url(gateway_url).await?;
        reporter.status("Mode", &format!("remote ({gateway_url})"));
        Mode::Remote { gateway_url }
    } else {
        let mcp_bin = match super::serve_cmd::find_mcp_binary()? {
            Some(p) => p,
            None => {
                reporter.status("hs-mcp", "not found locally, downloading from GitHub...");
                download_mcp_binary(reporter).await?
            }
        };
        reporter.status(
            "Mode",
            &format!(
                "local (hs-mcp at {}, memory at {LOCAL_MEMORY_URL})",
                mcp_bin.display()
            ),
        );
        Mode::Local {
            mcp_bin,
            env: secrets_as_json()?,
        }
    };

    let mut files_written = 0usize;
    for (target, path) in &targets {
        let mut config = read_config(path)?;
        let (written, by_hand) = write_servers(&mut config, *target, plan(*target, &mode))?;
        for server in &by_hand {
            reporter.status(
                "Skipped",
                &format!(
                    "{}: {} is not written, because its file takes commands only: {}",
                    target.label(),
                    server.name,
                    server.by_hand
                ),
            );
        }
        if written.is_empty() {
            continue;
        }
        write_config(path, &config)?;
        files_written += 1;
        reporter.status(
            "Installed",
            &format!(
                "{} ({}): {}",
                target.label(),
                path.display(),
                written.join(", ")
            ),
        );
    }

    if files_written == 0 {
        anyhow::bail!(
            "Nothing was written: no client named by --client takes these servers from its file"
        );
    }
    reporter.finish("MCP servers configured. Restart your client to pick up the changes.");
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
    let install_dir = match std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
    {
        Some(dir) => dir,
        None => hs_common::home_dir()?.join(".local/bin"),
    };
    let install_path = install_dir.join("hs-mcp");
    installer
        .install(&prepared, &install_path, reporter)
        .await?;

    reporter.status("Installed", &format!("hs-mcp → {}", install_path.display()));
    Ok(install_path)
}

// ── Uninstall ──────────────────────────────────────────────────

async fn cmd_uninstall(client: McpClient, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let targets = targets(&client);
    if targets.is_empty() {
        anyhow::bail!("No supported config path found for this platform");
    }

    for (target, path) in &targets {
        let name = target.label();
        if !path.exists() {
            reporter.status("Skipped", &format!("{name} (no config file)"));
            continue;
        }

        let mut config = read_config(path)?;
        let removed = remove_servers(&mut config, *target);
        if removed.is_empty() {
            reporter.status("Skipped", &format!("{name} (not configured)"));
            continue;
        }
        write_config(path, &config)?;
        reporter.status(
            "Removed",
            &format!("{name} ({}): {}", path.display(), removed.join(", ")),
        );
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

    fn local() -> Mode {
        Mode::Local {
            mcp_bin: PathBuf::from("/opt/hs/hs-mcp"),
            env: Some(serde_json::json!({ "HS_BACKEND_TOKEN": "t0ken" })),
        }
    }

    fn remote() -> Mode {
        Mode::Remote {
            gateway_url: "https://cloud.example.com/".into(),
        }
    }

    /// Install into `config` and return it with the names written and the
    /// names left for the user to add by hand.
    fn install(
        mut config: serde_json::Value,
        target: Target,
        mode: &Mode,
    ) -> (serde_json::Value, Vec<&'static str>, Vec<&'static str>) {
        let (written, by_hand) = write_servers(&mut config, target, plan(target, mode)).unwrap();
        let by_hand = by_hand.iter().map(|server| server.name).collect();
        (config, written, by_hand)
    }

    /// An agent needs both servers: hs-mcp has no search of its own, and
    /// the memory server has no `cite`. A client set up with one of them
    /// can find a passage and not show it, or the other way round.
    #[test]
    fn a_local_install_gives_claude_code_both_servers() {
        let before = serde_json::json!({
            "theme": "dark",
            "mcpServers": { "other": { "command": "/bin/other" } }
        });
        let (config, written, by_hand) = install(before, Target::Code, &local());
        assert_eq!(written, ["home-still", "myelin"]);
        assert!(by_hand.is_empty());
        assert_eq!(
            config,
            serde_json::json!({
                "theme": "dark",
                "mcpServers": {
                    "other": { "command": "/bin/other" },
                    "home-still": {
                        "command": "/opt/hs/hs-mcp",
                        "env": { "HS_BACKEND_TOKEN": "t0ken" }
                    },
                    "myelin": { "type": "http", "url": "http://127.0.0.1:7463/mcp" }
                }
            })
        );
    }

    /// The gateway serves the two servers on two paths, and a trailing
    /// slash on its URL must not double up in either.
    #[test]
    fn a_remote_install_points_each_server_at_its_own_gateway_path() {
        let (config, written, _) = install(serde_json::json!({}), Target::Code, &remote());
        assert_eq!(written, ["home-still", "myelin"]);
        assert_eq!(
            config["mcpServers"],
            serde_json::json!({
                "home-still": { "type": "http", "url": "https://cloud.example.com/mcp" },
                "myelin": { "type": "http", "url": "https://cloud.example.com/memory/mcp" }
            })
        );
    }

    #[test]
    fn opencode_gets_its_own_entry_shapes_under_its_own_key() {
        let (config, _, _) = install(serde_json::json!({}), Target::OpenCode, &local());
        assert_eq!(
            config["mcp"],
            serde_json::json!({
                "home-still": {
                    "type": "local",
                    "command": ["/opt/hs/hs-mcp"],
                    "enabled": true,
                    "environment": { "HS_BACKEND_TOKEN": "t0ken" }
                },
                "myelin": {
                    "type": "remote",
                    "url": "http://127.0.0.1:7463/mcp",
                    "enabled": true
                }
            })
        );
        assert!(config.get("mcpServers").is_none());
    }

    /// Claude Desktop reads only commands from its file. A URL entry there
    /// is an invalid one, so the server it cannot name is reported for the
    /// user to add in the app and nothing is written for it.
    #[test]
    fn claude_desktop_is_never_given_a_url_entry() {
        let (config, written, by_hand) = install(serde_json::json!({}), Target::Desktop, &local());
        assert_eq!(written, ["home-still"]);
        assert_eq!(by_hand, ["myelin"]);
        assert_eq!(
            config["mcpServers"],
            serde_json::json!({
                "home-still": {
                    "command": "/opt/hs/hs-mcp",
                    "env": { "HS_BACKEND_TOKEN": "t0ken" }
                }
            })
        );

        let (config, written, by_hand) = install(serde_json::json!({}), Target::Desktop, &remote());
        assert!(written.is_empty());
        assert_eq!(by_hand, ["home-still", "myelin"]);
        assert_eq!(config["mcpServers"], serde_json::json!({}));
    }

    #[test]
    fn installing_again_replaces_the_entries_instead_of_adding_to_them() {
        let (config, _, _) = install(serde_json::json!({}), Target::Code, &local());
        let (config, _, _) = install(config, Target::Code, &remote());
        assert_eq!(
            config["mcpServers"],
            serde_json::json!({
                "home-still": { "type": "http", "url": "https://cloud.example.com/mcp" },
                "myelin": { "type": "http", "url": "https://cloud.example.com/memory/mcp" }
            })
        );
    }

    #[test]
    fn uninstall_removes_both_servers_and_nothing_else() {
        for target in [Target::Code, Target::OpenCode] {
            let other = serde_json::json!({ "command": "/bin/other" });
            let mut before = serde_json::json!({ "theme": "dark" });
            before[target.servers_key()] = serde_json::json!({ "other": other });
            let (mut config, _, _) = install(before.clone(), target, &local());

            assert_eq!(
                remove_servers(&mut config, target),
                ["home-still", "myelin"]
            );
            assert_eq!(config, before, "{target:?}");
            assert!(remove_servers(&mut config, target).is_empty());
        }
        assert!(remove_servers(&mut serde_json::json!({}), Target::Code).is_empty());
    }
}
