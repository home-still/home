use std::sync::Arc;

use anyhow::Result;
use hs_common::reporter::Reporter;

/// Restart all running home-still services.
///
/// Detects what's running and restarts each:
/// 1. System services (systemd/launchd) — scribe, distill, mcp
/// 2. Scribe watch daemon (PID-tracked)
/// 3. Distill index daemon (PID-tracked)
/// 4. Docker compose containers (Ollama, Qdrant)
pub async fn run(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let mut restarted = 0u32;

    // 1. System services (hs serve scribe/distill/mcp)
    for svc in &["scribe", "distill", "mcp"] {
        if restart_system_service(svc, reporter).await? {
            restarted += 1;
        }
    }

    // 2. Scribe watch daemon
    if restart_scribe_watcher(reporter)? {
        restarted += 1;
    }

    // 3. Distill index daemon
    if restart_index_daemon(reporter).await? {
        restarted += 1;
    }

    // 4. Docker compose services
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
        if let Ok(output) = std::process::Command::new("systemctl")
            .args(["is-active", &service_name])
            .output()
        {
            let status = String::from_utf8_lossy(&output.stdout);
            if status.trim() == "active" {
                reporter.status("Restart", &service_name);
                let result = tokio::process::Command::new("sudo")
                    .args(["systemctl", "restart", &service_name])
                    .status()
                    .await?;
                if result.success() {
                    reporter.status("OK", &format!("{service_name} restarted"));
                    return Ok(true);
                } else {
                    reporter.warn(&format!("Failed to restart {service_name}"));
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

// ── Scribe watch daemon ────────────────────────────────────────

fn restart_scribe_watcher(reporter: &Arc<dyn Reporter>) -> Result<bool> {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
    let watch_dir = &scribe_cfg.watch_dir;

    // Check if watcher is running
    let pid_path = crate::daemon::pid_file_path(watch_dir);
    let pid = crate::daemon::read_pid(&pid_path);

    match pid {
        Some(pid) if crate::daemon::is_process_alive(pid) => {
            reporter.status("Restart", &format!("scribe watcher (PID {pid})"));
            crate::daemon::stop_daemon(watch_dir)?;
            crate::daemon::spawn_daemon(None, None, None)?;
            reporter.status("OK", "scribe watcher restarted");
            Ok(true)
        }
        _ => {
            // Watcher is dead — start it if there's a watch directory configured
            if watch_dir.exists() {
                reporter.status("Start", "scribe watcher (was stopped)");
                crate::daemon::spawn_daemon(None, None, None)?;
                reporter.status("OK", "scribe watcher started");
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
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

    let compose_files: Vec<(&str, std::path::PathBuf)> = vec![
        ("scribe", hidden.join("docker-compose.yml")),
        ("distill", hidden.join("docker-compose-distill.yml")),
    ];

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
        let _ = compose.run(&["-f", &cf, "restart"]).await;
        reporter.status("OK", &format!("{name} containers restarted"));
        count += 1;
    }

    Ok(count)
}
