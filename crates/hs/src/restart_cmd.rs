//! Restart the services that actually run this host's binaries.
//!
//! There is no hardcoded service-name table. Units are discovered at runtime
//! (systemd system + `--user`, launchd on macOS) and selected by the
//! executable their `ExecStart` / `ProgramArguments` points at, so a replaced
//! binary implies exactly the set of units that must be bounced — including
//! units whose names this code has never heard of.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use hs_common::reporter::Reporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitScope {
    System,
    User,
    #[cfg(target_os = "macos")]
    Launchd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceUnit {
    scope: UnitScope,
    /// `hs-serve-mcp.service` | `hs-scribe-watch-events.service` | `com.home-still.scribe`
    name: String,
    /// The binary the unit runs.
    exec_path: PathBuf,
    /// The unit's `argv[]`, used to probe a squatted port before restarting.
    exec_argv: String,
    active: bool,
    /// systemd `UnitFileState` is `enabled`/`static`; always false for launchd.
    enabled: bool,
    /// systemd `Type=oneshot` — timer-driven jobs, never bounced.
    oneshot: bool,
}

/// Restart every home-still unit running a local binary, the distill index
/// daemon, and the compose containers. Called by `hs restart`.
pub async fn run(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let binaries = installed_binaries();
    let (mut restarted, mut failures) = restart_units(&binaries, reporter).await;

    match restart_index_daemon(reporter).await {
        Ok(true) => restarted += 1,
        Ok(false) => {}
        Err(e) => failures.push(format!("distill indexer: {e:#}")),
    }

    restarted += restart_compose_services(reporter).await?;

    finish(restarted, failures, reporter)
}

/// Restart only the units running one of `replaced`. Called by `hs upgrade`
/// once the new binaries are on disk.
pub async fn after_upgrade(replaced: &[PathBuf], reporter: &Arc<dyn Reporter>) -> Result<()> {
    let (mut restarted, mut failures) = restart_units(replaced, reporter).await;

    // The index daemon is started as a child of `hs`, so only a replaced `hs`
    // changes what it would exec.
    if std::env::current_exe()
        .ok()
        .is_some_and(|exe| is_replaced(&exe, replaced))
    {
        match restart_index_daemon(reporter).await {
            Ok(true) => restarted += 1,
            Ok(false) => {}
            Err(e) => failures.push(format!("distill indexer: {e:#}")),
        }
    }

    finish(restarted, failures, reporter)
}

fn finish(restarted: u32, failures: Vec<String>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    if failures.is_empty() {
        if restarted == 0 {
            reporter.finish("No running services found to restart");
        } else {
            reporter.finish(&format!("Restarted {restarted} service(s)"));
        }
        return Ok(());
    }

    // A restart that did not happen is a failed upgrade, not a footnote: the
    // binaries are new on disk and old in memory.
    bail!(
        "{} service restart(s) failed — the new binaries are on disk but not running:\n  {}",
        failures.len(),
        failures.join("\n  ")
    )
}

// ── Discovery ──────────────────────────────────────────────────

/// Binaries installed on this host and therefore worth matching against.
fn installed_binaries() -> Vec<PathBuf> {
    let mut binaries = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        binaries.push(exe);
    }
    for name in [
        "hs-gateway",
        "hs-mcp",
        "hs-scribe-server",
        "hs-distill-server",
    ] {
        if let Some(path) = crate::upgrade_cmd::find_companion_binary(name) {
            binaries.push(path);
        }
    }
    binaries
}

async fn restart_units(binaries: &[PathBuf], reporter: &Arc<dyn Reporter>) -> (u32, Vec<String>) {
    let mut units = discover_system_units().await;
    units.extend(discover_user_units(reporter).await);
    #[cfg(target_os = "macos")]
    units.extend(discover_launchd_units());

    let mut restarted = 0u32;
    let mut failures = Vec::new();

    for unit in select_units(units, binaries) {
        if !unit.active {
            if unit.enabled {
                reporter.warn(&format!(
                    "{}: enabled but inactive — binary upgraded, unit left stopped \
                     (start it if it should be running)",
                    unit.name
                ));
            }
            continue;
        }

        // A userspace process squatting on the unit's port makes `systemctl
        // restart` fail with EADDRINUSE; name it before we try.
        #[cfg(target_os = "linux")]
        if let Some(port) = parse_serve_port(&unit.exec_argv) {
            if let Some(rogue_pid) = port_holder_not_under_unit(&unit.name, port) {
                reporter.warn(&format!(
                    "{} restart will likely fail: PID {rogue_pid} holds port {port} and is NOT \
                     managed by systemd. Kill it first: `kill {rogue_pid}`, then retry.",
                    unit.name
                ));
            }
        }

        match restart_unit(&unit, reporter).await {
            Ok(()) => restarted += 1,
            Err(failure) => failures.push(failure),
        }
    }

    (restarted, failures)
}

/// Units that must be bounced for `binaries`: running a replaced binary and
/// not timer-driven. Kept pure so the selection rules are testable.
fn select_units(units: Vec<ServiceUnit>, binaries: &[PathBuf]) -> Vec<ServiceUnit> {
    units
        .into_iter()
        .filter(|unit| !unit.oneshot && matches_replaced(unit, binaries))
        .collect()
}

#[cfg(target_os = "linux")]
async fn discover_system_units() -> Vec<ServiceUnit> {
    discover_systemd_units(false).await.unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
async fn discover_system_units() -> Vec<ServiceUnit> {
    Vec::new()
}

#[cfg(target_os = "linux")]
async fn discover_user_units(reporter: &Arc<dyn Reporter>) -> Vec<ServiceUnit> {
    match discover_systemd_units(true).await {
        Some(units) => units,
        None => {
            reporter.warn("systemctl --user unavailable: user units not discovered");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn discover_user_units(_reporter: &Arc<dyn Reporter>) -> Vec<ServiceUnit> {
    Vec::new()
}

#[cfg(target_os = "linux")]
async fn discover_systemd_units(user_scope: bool) -> Option<Vec<ServiceUnit>> {
    let mut list = systemctl(user_scope);
    list.args([
        "list-units",
        "--type=service",
        "--all",
        "--no-legend",
        "hs-*",
        "home-still*",
    ]);
    let output = list.output().await.ok()?;
    if !output.status.success() {
        return None;
    }

    let names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.ends_with(".service"))
        .map(str::to_string)
        .collect();

    let mut units = Vec::with_capacity(names.len());
    for name in names {
        if let Some(unit) = show_systemd_unit(user_scope, &name).await {
            units.push(unit);
        }
    }
    Some(units)
}

#[cfg(target_os = "linux")]
async fn show_systemd_unit(user_scope: bool, name: &str) -> Option<ServiceUnit> {
    let mut cmd = systemctl(user_scope);
    cmd.args([
        "show",
        name,
        "-p",
        "ExecStart",
        "-p",
        "ActiveState",
        "-p",
        "UnitFileState",
        "-p",
        "Type",
    ]);
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let exec_start = property(&stdout, "ExecStart")?;
    Some(ServiceUnit {
        scope: if user_scope {
            UnitScope::User
        } else {
            UnitScope::System
        },
        name: name.to_string(),
        exec_path: parse_exec_start_path(exec_start)?,
        exec_argv: parse_exec_start_argv(exec_start).unwrap_or_default(),
        active: property(&stdout, "ActiveState") == Some("active"),
        enabled: matches!(
            property(&stdout, "UnitFileState"),
            Some("enabled") | Some("static")
        ),
        oneshot: property(&stdout, "Type") == Some("oneshot"),
    })
}

#[cfg(target_os = "linux")]
fn systemctl(user_scope: bool) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("systemctl");
    if user_scope {
        cmd.arg("--user");
    }
    cmd
}

#[cfg(target_os = "macos")]
fn discover_launchd_units() -> Vec<ServiceUnit> {
    let Ok(output) = std::process::Command::new("launchctl").arg("list").output() else {
        return Vec::new();
    };

    home_still_launchd_entries(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .filter_map(|(pid, label)| {
            let plist = dirs::home_dir()?
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            let exec_path = parse_plist_program_path(&std::fs::read_to_string(plist).ok()?)?;
            Some(ServiceUnit {
                scope: UnitScope::Launchd,
                name: label,
                exec_path,
                exec_argv: String::new(),
                active: pid.is_some(),
                enabled: false,
                oneshot: false,
            })
        })
        .collect()
}

// ── Restart + verify ───────────────────────────────────────────

/// Restart one unit and prove it came back on the expected binary. The `Err`
/// string is the operator-facing reason.
async fn restart_unit(unit: &ServiceUnit, reporter: &Arc<dyn Reporter>) -> Result<(), String> {
    reporter.status("Restart", &unit.name);

    let result = match unit.scope {
        UnitScope::System | UnitScope::User => restart_systemd_unit(unit).await,
        #[cfg(target_os = "macos")]
        UnitScope::Launchd => restart_launchd_unit(unit).await,
    };

    if result.is_ok() {
        reporter.status("OK", &format!("{} restarted", unit.name));
    }
    result
}

async fn restart_systemd_unit(unit: &ServiceUnit) -> Result<(), String> {
    let mut cmd = if unit.scope == UnitScope::System {
        if !sudo_can_restart(&unit.name).await {
            return Err(format!(
                "{}: `sudo systemctl restart {}` is not permitted without a password — \
                 configure /etc/sudoers.d/hs-systemd",
                unit.name, unit.name
            ));
        }
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.args(["-n", "systemctl", "restart", &unit.name]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("systemctl");
        cmd.args(["--user", "restart", &unit.name]);
        cmd
    };

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("{}: systemctl restart could not run: {e}", unit.name))?;
    if !output.status.success() {
        return Err(format!(
            "{}: `systemctl restart` exited {:?}: {} — try `journalctl -u {} -n 50`",
            unit.name,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
            unit.name
        ));
    }

    verify_systemd_unit(unit).await
}

/// Re-read the unit and require it to be `active` on the binary we are meant
/// to be running. Without this an upgrade can replace a binary, fail to
/// re-exec it, and still print OK.
async fn verify_systemd_unit(unit: &ServiceUnit) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("systemctl");
    if unit.scope == UnitScope::User {
        cmd.arg("--user");
    }
    cmd.args(["show", &unit.name, "-p", "ActiveState", "-p", "MainPID"]);
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("{}: systemctl show failed: {e}", unit.name))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let state = property(&stdout, "ActiveState").unwrap_or("unknown");
    let pid = property(&stdout, "MainPID").unwrap_or("0");

    if state != "active" {
        return Err(format!(
            "{}: still not running the new binary after restart (ActiveState={state}, MainPID={pid})",
            unit.name
        ));
    }

    // `MainPID=0` means nothing is running under the unit (fine for units we
    // deliberately left stopped); otherwise the exe must be the new binary.
    #[cfg(target_os = "linux")]
    if pid != "0" {
        let expected = canonical_or(&unit.exec_path);
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        let running = exe.as_deref().map(canonical_or);
        if running.as_deref() != Some(expected.as_path()) {
            return Err(format!(
                "{}: still not running the new binary after restart (ActiveState={state}, \
                 MainPID={pid}, exe={})",
                unit.name,
                exe.map(|p| p.display().to_string())
                    .unwrap_or_else(|| "unreadable".to_string())
            ));
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
async fn restart_launchd_unit(unit: &ServiceUnit) -> Result<(), String> {
    let target = launchd_target(&unit.name);
    let output = tokio::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .await
        .map_err(|e| format!("{}: launchctl kickstart could not run: {e}", unit.name))?;
    if !output.status.success() {
        return Err(format!(
            "{}: `launchctl kickstart -k {target}` exited {:?}: {}",
            unit.name,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    match launchd_pid(&unit.name).await {
        Some(_) => Ok(()),
        None => Err(format!(
            "{}: still not running the new binary after restart (no pid)",
            unit.name
        )),
    }
}

#[cfg(target_os = "macos")]
fn launchd_target(label: &str) -> String {
    format!("gui/{}/{}", crate::scribe_inbox_install::users_uid(), label)
}

#[cfg(target_os = "macos")]
async fn launchd_pid(label: &str) -> Option<u32> {
    let output = tokio::process::Command::new("launchctl")
        .args(["print", &launchd_target(label)])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_launchd_print_pid(&String::from_utf8_lossy(&output.stdout))
}

// ── Pure helpers ───────────────────────────────────────────────

/// `Key=Value` line from `systemctl show` output.
fn property<'a>(show_output: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    show_output
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
}

/// Field of the `{ path=… ; argv[]=… ; … }` struct `systemctl show -p
/// ExecStart` prints.
#[cfg(any(target_os = "linux", test))]
fn exec_start_field<'a>(exec_start: &'a str, key: &str) -> Option<&'a str> {
    let start = exec_start.find(key)? + key.len();
    let rest = &exec_start[start..];
    let value = rest.split(" ;").next().unwrap_or(rest).trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_exec_start_path(exec_start: &str) -> Option<PathBuf> {
    Some(PathBuf::from(exec_start_field(exec_start, "path=")?))
}

#[cfg(any(target_os = "linux", test))]
fn parse_exec_start_argv(exec_start: &str) -> Option<String> {
    Some(exec_start_field(exec_start, "argv[]=")?.to_string())
}

/// Port a unit's `hs serve <kind> --port N` argv binds. Taken from the unit
/// itself: a name→port table drifted (`hs-serve-scribe-olmocr` binds 7435, not
/// 7433).
#[cfg(any(target_os = "linux", test))]
fn parse_serve_port(argv: &str) -> Option<u16> {
    let tokens: Vec<&str> = argv.split_whitespace().collect();
    let exe = tokens.first()?;
    if !(exe.ends_with("/hs") || *exe == "hs") {
        return None;
    }
    let serve_at = tokens.iter().position(|t| *t == "serve")?;
    let port_at = tokens.iter().position(|t| *t == "--port")?;
    if serve_at + 1 >= port_at {
        return None;
    }
    tokens.get(port_at + 1)?.parse().ok()
}

/// Exact path equality — never a substring, so a dev build in
/// `target/release/` is not mistaken for the installed binary.
fn is_replaced(path: &Path, binaries: &[PathBuf]) -> bool {
    let path = canonical_or(path);
    binaries.iter().any(|binary| canonical_or(binary) == path)
}

fn matches_replaced(unit: &ServiceUnit, binaries: &[PathBuf]) -> bool {
    is_replaced(&unit.exec_path, binaries)
}

fn canonical_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(any(target_os = "macos", test))]
fn is_home_still_label(label: &str) -> bool {
    label.starts_with("com.home-still.") || label.starts_with("io.home-still.")
}

/// `launchctl list` rows (`PID Status Label`) that belong to home-still.
///
/// Selection is by the label's own domain prefix — a label is addressed by
/// identity, never by `contains`, which is what made the old substring check
/// bounce `com.home-still.scribe-watch-events` when it meant
/// `com.home-still.scribe`.
#[cfg(any(target_os = "macos", test))]
fn home_still_launchd_entries(stdout: &str) -> Vec<(Option<u32>, String)> {
    parse_launchctl_list(stdout)
        .into_iter()
        .filter(|(_, _, label)| is_home_still_label(label))
        .map(|(pid, _, label)| (pid, label))
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchctl_list(stdout: &str) -> Vec<(Option<u32>, i32, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid = cols.next()?;
            let status = cols.next()?;
            let label = cols.next()?;
            if pid == "PID" {
                return None; // header row
            }
            Some((pid.parse().ok(), status.parse().ok()?, label.to_string()))
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn parse_plist_program_path(plist: &str) -> Option<PathBuf> {
    let array = plist.split("<key>ProgramArguments</key>").nth(1)?;
    let array = array.split("<array>").nth(1)?;
    let first = array.split("<string>").nth(1)?;
    let value = first.split("</string>").next()?.trim();
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_print_pid(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
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

// ── Helpers ────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    const HS: &str = "/home/ladvien/.local/bin/hs";

    fn unit(scope: UnitScope, name: &str, exec: &str) -> ServiceUnit {
        ServiceUnit {
            scope,
            name: name.to_string(),
            exec_path: PathBuf::from(exec),
            exec_argv: String::new(),
            active: true,
            enabled: true,
            oneshot: false,
        }
    }

    #[test]
    fn parse_exec_start_path_picks_path() {
        let big = "{ path=/home/ladvien/.local/bin/hs ; \
                   argv[]=/home/ladvien/.local/bin/hs serve scribe --port 7435 ; \
                   ignore_errors=no ; start_time=[Wed 2026-09-16 06:48:59 CDT] ; \
                   stop_time=[n/a] ; pid=973 ; code=(null) ; status=0/0 }";
        assert_eq!(parse_exec_start_path(big), Some(PathBuf::from(HS)));

        let shell = "{ path=/bin/sh ; argv[]=/bin/sh -c 'exec hs serve mcp' ; ignore_errors=no ; }";
        assert_eq!(parse_exec_start_path(shell), Some(PathBuf::from("/bin/sh")));

        assert_eq!(parse_exec_start_path(""), None);
    }

    #[test]
    fn parse_exec_start_argv_and_port() {
        let big = "{ path=/home/ladvien/.local/bin/hs ; \
                   argv[]=/home/ladvien/.local/bin/hs serve scribe --port 7435 ; \
                   ignore_errors=no ; }";
        let argv = parse_exec_start_argv(big).expect("argv");
        assert_eq!(argv, "/home/ladvien/.local/bin/hs serve scribe --port 7435");
        assert_eq!(parse_serve_port(&argv), Some(7435));

        // Someone else's server: not ours to probe.
        assert_eq!(parse_serve_port("/usr/bin/vllm serve --port 8081"), None);
        // The watcher daemons bind nothing.
        assert_eq!(
            parse_serve_port("/home/ladvien/.local/bin/hs distill watch-events"),
            None
        );
    }

    #[test]
    fn matches_replaced_requires_exact_binary() {
        let installed = PathBuf::from(HS);
        let dev = PathBuf::from("/home/ladvien/home-still/target/release/hs");

        assert!(matches_replaced(
            &unit(UnitScope::System, "hs-serve-mcp.service", HS),
            std::slice::from_ref(&installed)
        ));
        // The dev build in target/release must not be matched by substring.
        assert!(!matches_replaced(
            &unit(
                UnitScope::System,
                "hs-distill-watch.service",
                "/home/ladvien/home-still/target/release/hs"
            ),
            &[installed]
        ));
        assert!(matches_replaced(
            &unit(
                UnitScope::System,
                "hs-distill-watch.service",
                "/home/ladvien/home-still/target/release/hs"
            ),
            &[dev]
        ));
    }

    #[test]
    fn parse_launchctl_list_reads_pid_status_label() {
        let rows = parse_launchctl_list(
            "PID\tStatus\tLabel\n-\t2\tcom.home-still.scribe-autotune\n1340\t0\tcom.home-still.scribe\n",
        );
        assert_eq!(
            rows,
            vec![
                (None, 2, "com.home-still.scribe-autotune".to_string()),
                (Some(1340), 0, "com.home-still.scribe".to_string()),
            ]
        );
    }

    #[test]
    fn parse_plist_program_path_reads_first_string() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.home-still.scribe</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/rebekahbrittain/.local/bin/hs</string>
        <string>serve</string>
        <string>scribe</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#;
        assert_eq!(
            parse_plist_program_path(plist),
            Some(PathBuf::from("/Users/rebekahbrittain/.local/bin/hs"))
        );
        assert_eq!(parse_plist_program_path("<plist></plist>"), None);
    }

    #[test]
    fn parse_launchd_print_pid_reads_pid_line() {
        let out = "gui/501/com.home-still.scribe = {\n\tactive count = 1\n\tpath = \
                   /Users/x/Library/LaunchAgents/com.home-still.scribe.plist\n\tstate = running\n\n\t\
                   program = /Users/x/.local/bin/hs\n\tpid = 4242\n}\n";
        assert_eq!(parse_launchd_print_pid(out), Some(4242));
        assert_eq!(
            parse_launchd_print_pid("gui/501/com.home-still.scribe = { state = exited }"),
            None
        );
    }

    #[test]
    fn label_prefix_match_is_exact() {
        let listed = "PID\tStatus\tLabel\n1200\t0\tcom.example.home-still.scribe\n\
                      1340\t0\tcom.home-still.scribe\n\
                      1350\t0\tcom.home-still.scribe-watch-events\n";
        let entries = home_still_launchd_entries(listed);

        // A label that merely contains the domain is not ours.
        assert!(!entries.iter().any(|(_, label)| label.contains("example")));
        // Each label addresses exactly one unit; `…scribe` is not `…scribe-watch-events`.
        assert_eq!(
            entries,
            vec![
                (Some(1340), "com.home-still.scribe".to_string()),
                (Some(1350), "com.home-still.scribe-watch-events".to_string()),
            ]
        );
    }

    #[test]
    fn oneshot_units_are_not_selected() {
        let binaries = vec![PathBuf::from(HS)];
        let mut reconcile = unit(UnitScope::User, "hs-distill-reconcile.service", HS);
        reconcile.oneshot = true;
        let long_running = unit(UnitScope::System, "hs-serve-mcp.service", HS);

        let selected = select_units(vec![reconcile, long_running], &binaries);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "hs-serve-mcp.service");
    }
}
