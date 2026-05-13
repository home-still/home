use std::sync::Arc;

use anyhow::Result;
use hs_common::reporter::Reporter;

/// Restart all running home-still services.
///
/// Detects what's running and restarts each:
/// 1. System services (systemd/launchd) — scribe, distill, mcp
/// 2. Distill index daemon (PID-tracked)
/// 3. Docker compose containers (Ollama, Qdrant)
pub async fn run(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let mut restarted = 0u32;

    // 1. System services (hs serve scribe/distill/mcp)
    for svc in &["scribe", "distill", "mcp"] {
        if restart_system_service(svc, reporter).await? {
            restarted += 1;
        }
    }

    // 2. Distill index daemon
    if restart_index_daemon(reporter).await? {
        restarted += 1;
    }

    // 3. Docker compose services
    restarted += restart_compose_services(reporter).await?;

    if restarted == 0 {
        reporter.finish("No running services found to restart");
    } else {
        reporter.finish(&format!("Restarted {restarted} service(s)"));
    }
    Ok(())
}

// ── System services (systemd / launchd) ────────────────────────

async fn restart_system_service(service_type: &str, reporter: &Arc<dyn Reporter>) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let service_name = format!("hs-serve-{service_type}");

        // Skip silently if the unit isn't installed at all (covers hosts that
        // don't run this service type — most clients don't have hs-serve-*).
        let enabled = match std::process::Command::new("systemctl")
            .args(["is-enabled", &service_name])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => return Ok(false),
        };
        // `is-enabled` returns "enabled" / "disabled" / "static" / "" (not
        // found). We restart anything systemd knows about and that's enabled.
        // Disabled units are intentionally off — don't auto-bounce them.
        if enabled != "enabled" && enabled != "static" {
            return Ok(false);
        }

        // Earlier behavior: only restarted active units, so a unit that was
        // enabled-but-inactive (e.g. killed manually or never started after
        // install) ended up with the new binary on disk but no running
        // process. `systemctl restart` is idempotent — starts inactive units,
        // bounces active ones — so we use it unconditionally now.

        // Detect a rogue userspace process holding the unit's port. Common
        // failure mode: a `setsid nohup hs-mcp --serve` that bypassed
        // systemd. `systemctl start` would then fail with EADDRINUSE. Warn
        // loud and let the user clean it up — better than a silent restart
        // failure buried in journalctl.
        if let Some(port) = expected_service_port(service_type) {
            if let Some(rogue_pid) = port_holder_not_under_unit(&service_name, port) {
                reporter.warn(&format!(
                    "{service_name} restart will likely fail: PID {rogue_pid} holds port {port} \
                     and is NOT managed by systemd. Kill it first: `kill {rogue_pid}`, then retry."
                ));
            }
        }

        // Pre-check that `sudo systemctl restart <service>` is allowed
        // without a password. The sudoers entry here is typically scoped
        // to specific systemctl commands (e.g. `NOPASSWD: systemctl
        // restart hs-serve-*`), so a blanket `sudo -n true` probe gives
        // a false negative — `sudo -l <cmd>` checks the exact command's
        // policy and respects scoped NOPASSWD entries.
        if !sudo_can_restart(&service_name).await {
            reporter.warn(&format!(
                "{service_name}: skipping restart — `sudo systemctl restart {service_name}` not \
                 allowed without password. Configure /etc/sudoers.d/hs-systemd or run manually: \
                 `sudo systemctl restart {service_name}`"
            ));
            return Ok(false);
        }

        reporter.status("Restart", &service_name);
        let result = tokio::process::Command::new("sudo")
            .args(["-n", "systemctl", "restart", &service_name])
            .status()
            .await?;
        if result.success() {
            reporter.status("OK", &format!("{service_name} restarted"));
            return Ok(true);
        } else {
            reporter.warn(&format!(
                "Failed to restart {service_name} (exit {:?}). Try `sudo journalctl -u {} -n 50`",
                result.code(),
                service_name
            ));
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
            if stdout.contains(&label) {
                let plist_path = dirs::home_dir()
                    .unwrap_or_default()
                    .join("Library/LaunchAgents")
                    .join(format!("{label}.plist"));
                let plist_str = plist_path.to_string_lossy().to_string();

                reporter.status("Restart", &label);
                let _ = tokio::process::Command::new("launchctl")
                    .args(["unload", &plist_str])
                    .status()
                    .await;
                let result = tokio::process::Command::new("launchctl")
                    .args(["load", &plist_str])
                    .status()
                    .await?;
                if result.success() {
                    reporter.status("OK", &format!("{label} restarted"));
                    return Ok(true);
                } else {
                    reporter.warn(&format!("Failed to restart {label}"));
                }
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (service_type, reporter);

    Ok(false)
}

// ── Distill index daemon ───────────────────────────────────────

async fn restart_index_daemon(reporter: &Arc<dyn Reporter>) -> Result<bool> {
    let pid_path = dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
        .join("distill-index.pid");

    let pid = crate::daemon::read_pid(&pid_path);

    match pid {
        Some(pid) if crate::daemon::is_process_alive(pid) => {
            reporter.status("Restart", &format!("distill indexer (PID {pid})"));

            // Stop it (same pattern as distill_cmd::cmd_server_stop)
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                for _ in 0..50 {
                    if !crate::daemon::is_process_alive(pid) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if crate::daemon::is_process_alive(pid) {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
            crate::daemon::remove_pid_file(&pid_path);

            // Re-spawn
            crate::distill_cmd::ensure_index_running().await;
            reporter.status("OK", "distill indexer restarted");
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ── Docker compose containers ──────────────────────────────────

async fn restart_compose_services(reporter: &Arc<dyn Reporter>) -> Result<u32> {
    use hs_common::compose::ComposeCmd;

    let hidden = dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR);

    let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();

    let mut compose_files: Vec<(&str, std::path::PathBuf)> = Vec::new();
    if scribe_cfg.local_server {
        compose_files.push(("scribe", hidden.join("docker-compose.yml")));
    }
    compose_files.push(("distill", hidden.join("docker-compose-distill.yml")));

    let active: Vec<_> = compose_files
        .into_iter()
        .filter(|(_, p)| p.exists())
        .collect();

    if active.is_empty() {
        return Ok(0);
    }

    let compose = match ComposeCmd::detect().await {
        Some(c) => c,
        None => return Ok(0),
    };

    let mut count = 0u32;
    for (name, path) in &active {
        let cf = path.to_string_lossy().to_string();
        reporter.status("Restart", &format!("{name} containers"));
        let output = compose.run_capture(&["-f", &cf, "restart"]).await;
        match output {
            Ok(o) if o.status.success() => {
                reporter.status("OK", &format!("{name} containers restarted"));
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let errors = hs_common::compose::filter_compose_stderr(&stderr);
                if errors.is_empty() {
                    reporter.status("OK", &format!("{name} containers restarted"));
                } else {
                    reporter.warn(&format!("{name}: {}", errors.join("; ")));
                }
            }
            Err(e) => {
                reporter.warn(&format!("{name}: {e}"));
            }
        }
        count += 1;
    }

    Ok(count)
}

// ── Helpers for restart_system_service ─────────────────────────

/// Default port for each `hs serve <type>` variant. Used to detect rogue
/// userspace processes squatting on a unit's expected port. Keep in sync
/// with the DEFAULT_*_PORT constants in serve_cmd.rs.
#[cfg(target_os = "linux")]
fn expected_service_port(service_type: &str) -> Option<u16> {
    match service_type {
        "scribe" => Some(7433),
        "distill" => Some(7434),
        "mcp" => Some(7445),
        _ => None,
    }
}

/// Return PID of any process listening on `port` that is NOT in the
/// systemd cgroup of `unit_name`. Returns None if the port is free OR if
/// it's held by the legitimate systemd-managed process. Linux-only — uses
/// /proc/net/tcp + /proc/<pid>/cgroup, no shelling out.
#[cfg(target_os = "linux")]
fn port_holder_not_under_unit(unit_name: &str, port: u16) -> Option<u32> {
    use std::fs;
    let port_hex = format!("{:04X}", port);

    // Find any LISTEN socket bound to this port and read its inode.
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok())
        .flat_map(|s| s.lines().skip(1).map(String::from).collect::<Vec<_>>())
        .find_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // local_address is column 1 (after sl), state is column 3.
            // Format: "0100007F:1D2D 00000000:0000 0A 00000000:00000000 ...
            //  inode is column 9.
            if cols.len() < 10 {
                return None;
            }
            let local = cols.get(1)?;
            let state = cols.get(3)?;
            let inode_str = cols.get(9)?;
            // 0A = TCP_LISTEN
            if *state != "0A" {
                return None;
            }
            let local_port = local.split(':').nth(1)?;
            if !local_port.eq_ignore_ascii_case(&port_hex) {
                return None;
            }
            inode_str.parse::<u64>().ok()
        })?;

    // Walk /proc/<pid>/fd/ to find which PID owns that socket inode.
    let socket_target = format!("socket:[{}]", inode);
    let pids = fs::read_dir("/proc").ok()?;
    for entry in pids.flatten() {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let fd_dir = format!("/proc/{}/fd", pid);
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let target = match fs::read_link(fd.path()) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if target == socket_target {
                // Found the holding PID. Check if it's under the systemd unit.
                let cgroup =
                    fs::read_to_string(format!("/proc/{}/cgroup", pid)).unwrap_or_default();
                let in_unit = cgroup.contains(&format!("/{}.service/", unit_name))
                    || cgroup.contains(&format!("/{}.service\n", unit_name));
                if !in_unit {
                    return Some(pid);
                }
                return None;
            }
        }
    }
    None
}

/// Probe whether `sudo systemctl restart <service>` is allowed without
/// a password. Uses `sudo -nl <full-command>` which exits zero only if
/// the exact command is permitted under the current sudoers policy.
///
/// `sudo -l <cmd>` respects scoped NOPASSWD entries (e.g. `NOPASSWD:
/// /usr/bin/systemctl restart hs-serve-*`), so it returns true for the
/// real-world case where users grant per-service restart rights without
/// granting blanket NOPASSWD. Falling back to `sudo -n true` (the
/// previous probe) returned false for that case and caused
/// `hs upgrade` to skip every service restart with a misleading
/// "requires NOPASSWD or a tty" warning even when the actual restart
/// would have succeeded.
#[cfg(target_os = "linux")]
async fn sudo_can_restart(service_name: &str) -> bool {
    tokio::process::Command::new("sudo")
        .args(["-nl", "/usr/bin/systemctl", "restart", service_name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
