//! Cross-platform install/uninstall/status for the inbox watcher daemon.
//!
//! macOS: a user-level LaunchAgent at
//!   `~/Library/LaunchAgents/io.home-still.scribe-inbox.plist`
//! Linux: a user-level systemd unit at
//!   `~/.config/systemd/user/home-still-scribe-inbox.service`
//!
//! Both wrappers invoke the currently-running `hs` binary via
//! `std::env::current_exe()`, so whichever version you install with is
//! the version the daemon re-execs on login/restart.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::reporter::Reporter;

const SERVICE_LABEL: &str = "io.home-still.scribe-inbox";

pub async fn cmd_install(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current hs binary path")?;
    match std::env::consts::OS {
        "macos" => install_macos(reporter, &exe),
        "linux" => install_linux(reporter, &exe),
        other => anyhow::bail!("install not supported on this platform: {other}"),
    }
}

pub async fn cmd_uninstall(reporter: &Arc<dyn Reporter>) -> Result<()> {
    match std::env::consts::OS {
        "macos" => uninstall_macos(reporter),
        "linux" => uninstall_linux(reporter),
        other => anyhow::bail!("uninstall not supported on this platform: {other}"),
    }
}

pub async fn cmd_status(reporter: &Arc<dyn Reporter>) -> Result<()> {
    match std::env::consts::OS {
        "macos" => status_macos(reporter),
        "linux" => status_linux(reporter),
        other => anyhow::bail!("status not supported on this platform: {other}"),
    }
}

// ── macOS: LaunchAgent ───────────────────────────────────────────────

fn macos_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

fn macos_log_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home
        .join("Library")
        .join("Logs")
        .join("home-still-scribe-inbox.log"))
}

/// Render the LaunchAgent plist. `exe_path` must be absolute — it's baked
/// in as `ProgramArguments[0]`, so the installed daemon always runs the
/// same binary that installed it.
fn render_macos_plist(exe_path: &std::path::Path, log_path: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>scribe</string>
        <string>inbox</string>
        <string>daemon-child</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        exe = exe_path.display(),
        log = log_path.display(),
    )
}

fn install_macos(reporter: &Arc<dyn Reporter>, exe: &std::path::Path) -> Result<()> {
    let plist_path = macos_plist_path()?;
    let log_path = macos_log_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let body = render_macos_plist(exe, &log_path);
    std::fs::write(&plist_path, body).with_context(|| format!("write {}", plist_path.display()))?;
    reporter.status("Wrote", &plist_path.display().to_string());

    // Unload any stale instance, then bootstrap the new one. Use the
    // modern `bootstrap` / `bootout` syntax rather than deprecated `load`.
    let uid = users_uid();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{SERVICE_LABEL}")])
        .status();
    let status = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            plist_path.to_str().context("plist path not UTF-8")?,
        ])
        .status()
        .context("launchctl bootstrap")?;
    if !status.success() {
        anyhow::bail!(
            "launchctl bootstrap failed (exit {}); plist is at {}",
            status.code().unwrap_or(-1),
            plist_path.display(),
        );
    }
    reporter.status("Loaded", &format!("gui/{uid}/{SERVICE_LABEL}"));
    reporter.status("Log", &log_path.display().to_string());
    Ok(())
}

fn uninstall_macos(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let plist_path = macos_plist_path()?;
    let uid = users_uid();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{SERVICE_LABEL}")])
        .status();
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("remove {}", plist_path.display()))?;
        reporter.status("Removed", &plist_path.display().to_string());
    } else {
        reporter.status("Not installed", SERVICE_LABEL);
    }
    Ok(())
}

fn status_macos(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let uid = users_uid();
    let output = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{SERVICE_LABEL}")])
        .output()
        .context("launchctl print")?;
    if !output.status.success() {
        reporter.status("Not loaded", SERVICE_LABEL);
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Extract just the interesting lines so operators don't need to read the whole plist dump.
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("state") || t.starts_with("pid") || t.starts_with("last exit code") {
            reporter.status("Status", t);
        }
    }
    if let Ok(log) = macos_log_path() {
        reporter.status("Log", &log.display().to_string());
    }
    Ok(())
}

fn users_uid() -> u32 {
    // SAFETY: `geteuid` is a trivial read-only syscall with no out-params.
    unsafe { libc::geteuid() }
}

// ── Linux: systemd --user ───────────────────────────────────────────

fn linux_unit_name() -> &'static str {
    "home-still-scribe-inbox.service"
}

fn linux_unit_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home
        .join(".config")
        .join("systemd")
        .join("user")
        .join(linux_unit_name()))
}

fn render_linux_unit(exe_path: &std::path::Path) -> String {
    format!(
        r#"[Unit]
Description=home-still scribe inbox watcher
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe} scribe inbox daemon-child
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#,
        exe = exe_path.display(),
    )
}

fn install_linux(reporter: &Arc<dyn Reporter>, exe: &std::path::Path) -> Result<()> {
    let unit_path = linux_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&unit_path, render_linux_unit(exe))
        .with_context(|| format!("write {}", unit_path.display()))?;
    reporter.status("Wrote", &unit_path.display().to_string());

    let status = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("systemctl daemon-reload")?;
    if !status.success() {
        anyhow::bail!("systemctl --user daemon-reload failed");
    }
    let status = Command::new("systemctl")
        .args(["--user", "enable", "--now", linux_unit_name()])
        .status()
        .context("systemctl enable --now")?;
    if !status.success() {
        anyhow::bail!("systemctl --user enable --now failed");
    }
    reporter.status("Enabled", linux_unit_name());
    Ok(())
}

fn uninstall_linux(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", linux_unit_name()])
        .status();
    let unit_path = linux_unit_path()?;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("remove {}", unit_path.display()))?;
        reporter.status("Removed", &unit_path.display().to_string());
    } else {
        reporter.status("Not installed", linux_unit_name());
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}

fn status_linux(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let output = Command::new("systemctl")
        .args(["--user", "is-active", "--no-pager", linux_unit_name()])
        .output()
        .context("systemctl is-active")?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    reporter.status("Status", &state);
    // Also surface recent logs.
    let output = Command::new("journalctl")
        .args(["--user", "-u", linux_unit_name(), "-n", "5", "--no-pager"])
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().take(5) {
            reporter.status("Log", line);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn macos_plist_embeds_exe_path_and_args() {
        let plist = render_macos_plist(
            Path::new("/Users/me/.local/bin/hs"),
            Path::new("/Users/me/Library/Logs/x.log"),
        );
        assert!(plist.contains("<string>/Users/me/.local/bin/hs</string>"));
        assert!(plist.contains("<string>scribe</string>"));
        assert!(plist.contains("<string>inbox</string>"));
        assert!(plist.contains("<string>daemon-child</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("io.home-still.scribe-inbox"));
    }

    #[test]
    fn linux_unit_embeds_exe_path() {
        let unit = render_linux_unit(Path::new("/home/me/.local/bin/hs"));
        assert!(unit.contains("ExecStart=/home/me/.local/bin/hs scribe inbox daemon-child"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
