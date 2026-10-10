//! Restart the services that actually run this host's binaries.
//!
//! There is no hardcoded service-name table. Units are discovered at runtime
//! (systemd system + `--user`, launchd on macOS) and selected by the
//! executable their `ExecStart` / `ProgramArguments` points at, so a replaced
//! binary implies exactly the set of units that must be bounced — including
//! units whose names this code has never heard of.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "macos")]
use anyhow::Context;
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

    let (compose_restarted, compose_failures) = restart_compose_services(reporter).await?;
    restarted += compose_restarted;
    failures.extend(compose_failures);

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

/// Restart the discovered units running one of `binaries`. A scope whose
/// units cannot be listed (no user bus over ssh, `launchctl` failing) is a
/// failure of its own, reported with the rest: the other scopes are still
/// discovered and restarted, so one unreachable manager does not leave every
/// unit on the old code.
async fn restart_units(binaries: &[PathBuf], reporter: &Arc<dyn Reporter>) -> (u32, Vec<String>) {
    let mut restarted = 0u32;
    let mut failures = Vec::new();
    let mut units = Vec::new();
    let mut discovered = |what: &str, found: Result<Vec<ServiceUnit>>| match found {
        Ok(found) => units.extend(found),
        Err(e) => failures.push(format!("discovering {what}: {e:#}")),
    };
    discovered("system units", discover_system_units().await);
    discovered("user units", discover_user_units().await);
    #[cfg(target_os = "macos")]
    discovered("launchd jobs", discover_launchd_units());

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
async fn discover_system_units() -> Result<Vec<ServiceUnit>> {
    discover_systemd_units(false).await
}

#[cfg(not(target_os = "linux"))]
async fn discover_system_units() -> Result<Vec<ServiceUnit>> {
    Ok(Vec::new())
}

#[cfg(target_os = "linux")]
async fn discover_user_units() -> Result<Vec<ServiceUnit>> {
    discover_systemd_units(true).await
}

#[cfg(not(target_os = "linux"))]
async fn discover_user_units() -> Result<Vec<ServiceUnit>> {
    Ok(Vec::new())
}

#[cfg(any(target_os = "linux", test))]
fn systemctl_label(user_scope: bool) -> &'static str {
    if user_scope {
        "systemctl --user"
    } else {
        "systemctl"
    }
}

/// Unit names from `systemctl list-units` output. A host without a
/// `systemctl` binary has no systemd and so no units; every other failure
/// (non-zero exit, spawn error) is an error — an empty answer there would let
/// the restart phase report "nothing to restart" with the old code running.
#[cfg(any(target_os = "linux", test))]
fn interpret_list_units(
    run: std::io::Result<std::process::Output>,
    user_scope: bool,
) -> Result<Vec<String>> {
    let label = systemctl_label(user_scope);
    let output = match run {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("`{label} list-units` could not run: {e}"),
    };
    if !output.status.success() {
        bail!(
            "`{label} list-units` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // `--all` marks failed/not-found units with a leading "●"/"*".
            line.split_whitespace()
                .find(|col| col.ends_with(".service"))
        })
        .map(str::to_string)
        .collect())
}

/// Build the unit from `systemctl show` output for a unit that was listed.
/// `Ok(None)`: an empty `ExecStart=` — the unit has no main command and cannot
/// be running our binary. `Err`: the show failed or its output is missing or
/// garbled for a unit we were told exists.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn interpret_show_unit(
    user_scope: bool,
    name: &str,
    run: std::io::Result<std::process::Output>,
) -> Result<Option<ServiceUnit>> {
    let label = systemctl_label(user_scope);
    let output = run.map_err(|e| anyhow::anyhow!("{name}: `{label} show` could not run: {e}"))?;
    if !output.status.success() {
        bail!(
            "{name}: `{label} show` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // A unit that is not loaded (a failed transient unit whose file is gone,
    // or one masked to /dev/null) has no main command and cannot be running
    // our binary: the same case as an empty `ExecStart=`.
    if matches!(property(&stdout, "LoadState"), Some("not-found" | "masked")) {
        return Ok(None);
    }
    let Some(exec_start) = property(&stdout, "ExecStart") else {
        bail!("{name}: `{label} show` output has no ExecStart property");
    };
    if exec_start.trim().is_empty() {
        return Ok(None);
    }
    let Some(exec_path) = parse_exec_start_path(exec_start) else {
        bail!("{name}: cannot parse a binary path from ExecStart={exec_start:?}");
    };
    let Some(active_state) = property(&stdout, "ActiveState") else {
        bail!("{name}: `{label} show` output has no ActiveState property");
    };

    Ok(Some(ServiceUnit {
        scope: if user_scope {
            UnitScope::User
        } else {
            UnitScope::System
        },
        name: name.to_string(),
        exec_path,
        exec_argv: parse_exec_start_argv(exec_start).unwrap_or_default(),
        active: active_state == "active",
        enabled: matches!(
            property(&stdout, "UnitFileState"),
            Some("enabled") | Some("static")
        ),
        oneshot: property(&stdout, "Type") == Some("oneshot"),
    }))
}

/// The `systemctl show` arguments for one listed unit: exactly the
/// properties [`interpret_show_unit`] reads. A property that is not requested
/// reads as absent — `LoadState` missing here made every masked unit look
/// like garbled output on the rc.363 rollout.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn show_unit_args(name: &str) -> Vec<String> {
    let mut args = vec!["show".to_string(), name.to_string()];
    for property in [
        "LoadState",
        "ExecStart",
        "ActiveState",
        "UnitFileState",
        "Type",
    ] {
        args.push("-p".to_string());
        args.push(property.to_string());
    }
    args
}

#[cfg(target_os = "linux")]
async fn discover_systemd_units(user_scope: bool) -> Result<Vec<ServiceUnit>> {
    let mut list = systemctl(user_scope);
    list.args([
        "list-units",
        "--type=service",
        "--all",
        "--no-legend",
        "hs-*",
        "home-still*",
    ]);
    let names = interpret_list_units(list.output().await, user_scope)?;

    let mut units = Vec::with_capacity(names.len());
    for name in names {
        let mut show = systemctl(user_scope);
        show.args(show_unit_args(&name));
        if let Some(unit) = interpret_show_unit(user_scope, &name, show.output().await)? {
            units.push(unit);
        }
    }
    Ok(units)
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
fn discover_launchd_units() -> Result<Vec<ServiceUnit>> {
    let output = std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .context("`launchctl list` could not run")?;
    if !output.status.success() {
        bail!(
            "`launchctl list` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let home = dirs::home_dir().context("no home directory to locate ~/Library/LaunchAgents")?;

    let jobs = launchd_jobs(&String::from_utf8_lossy(&output.stdout), |label| {
        std::fs::read_to_string(launchd_plist_path(&home, label))
    })?;
    Ok(jobs
        .into_iter()
        .map(|job| ServiceUnit {
            scope: UnitScope::Launchd,
            name: job.label,
            exec_path: job.exec_path,
            exec_argv: String::new(),
            active: job.active,
            enabled: false,
            oneshot: false,
        })
        .collect())
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

/// How long a just-restarted unit is given to settle before its verification
/// is called a failure. `systemctl restart` returns as soon as the new process
/// is forked, and `/proc/<pid>/exe` is unreadable for a moment inside
/// `execve` — measured at ~20 ms.
const VERIFY_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const VERIFY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Re-read the unit until it is `active` on the binary we are meant to be
/// running. Without this an upgrade can replace a binary, fail to re-exec it,
/// and still print OK.
async fn verify_systemd_unit(unit: &ServiceUnit) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + VERIFY_SETTLE_TIMEOUT;
    loop {
        match inspect_systemd_unit(unit).await {
            Ok(()) => return Ok(()),
            Err(failure) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(failure);
                }
                tokio::time::sleep(VERIFY_POLL_INTERVAL).await;
            }
        }
    }
}

/// One verification attempt. `Err` is why this attempt did not pass.
async fn inspect_systemd_unit(unit: &ServiceUnit) -> Result<(), String> {
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

/// Restart a LaunchAgent so it runs the plist and binary on disk now.
///
/// `launchctl kickstart -k` relaunches the job launchd already has in memory:
/// an edited plist (environment, arguments) is not re-read. The job is booted
/// out and bootstrapped from its plist instead.
#[cfg(target_os = "macos")]
async fn restart_launchd_unit(unit: &ServiceUnit) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| {
        format!(
            "{}: no home directory to locate ~/Library/LaunchAgents",
            unit.name
        )
    })?;
    restart_launchd_job(
        &SystemLaunchctl,
        &format!("gui/{}", crate::scribe_inbox_install::users_uid()),
        &unit.name,
        &launchd_plist_path(&home, &unit.name),
        &unit.exec_path,
    )
    .await
}

/// Where the installers (`hs serve <svc> --install`, `hs scribe inbox
/// install`) write a label's plist.
#[cfg(target_os = "macos")]
fn launchd_plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

/// Exit of one `launchctl` / `ps` invocation.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchctlExit {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[cfg(any(target_os = "macos", test))]
impl LaunchctlExit {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// The commands [`restart_launchd_job`] issues, so the sequencing can be
/// driven without a launchd.
#[cfg(any(target_os = "macos", test))]
trait LaunchdRunner {
    /// Run `launchctl <args>`. `Err` is "could not run at all".
    async fn launchctl(&self, args: &[&str]) -> Result<LaunchctlExit, String>;
    /// Executable path of a running process.
    async fn exe_of(&self, pid: u32) -> Result<PathBuf, String>;
    async fn sleep(&self, duration: std::time::Duration);
}

#[cfg(target_os = "macos")]
struct SystemLaunchctl;

#[cfg(target_os = "macos")]
impl LaunchdRunner for SystemLaunchctl {
    async fn launchctl(&self, args: &[&str]) -> Result<LaunchctlExit, String> {
        let output = tokio::process::Command::new("launchctl")
            .args(args)
            .output()
            .await
            .map_err(|e| format!("`launchctl {}` could not run: {e}", args.join(" ")))?;
        Ok(LaunchctlExit {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// `ps -o comm=` prints a darwin process's full executable path.
    async fn exe_of(&self, pid: u32) -> Result<PathBuf, String> {
        let output = tokio::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .await
            .map_err(|e| format!("`ps -p {pid}` could not run: {e}"))?;
        let comm = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() || comm.is_empty() {
            return Err(format!("`ps -o comm= -p {pid}` found no process"));
        }
        Ok(PathBuf::from(comm))
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// `launchctl bootout` of a job that is not loaded: exit 3 (`No such
/// process`) or 113 (`Could not find service`). That is "was not running",
/// not a failure.
#[cfg(any(target_os = "macos", test))]
const LAUNCHCTL_NOT_LOADED: [i32; 2] = [3, 113];
/// `launchctl bootstrap` exit 5 (`Input/output error`): launchd has not
/// finished tearing down the job just booted out (BACKLOG P1-25). Transient.
#[cfg(any(target_os = "macos", test))]
const LAUNCHCTL_IO_ERROR: i32 = 5;
#[cfg(any(target_os = "macos", test))]
const BOOTSTRAP_ATTEMPTS: u32 = 5;
#[cfg(any(target_os = "macos", test))]
const BOOTSTRAP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a bootstrapped launchd job is given to get a pid. launchd
/// respawns a KeepAlive job on its own schedule (ThrottleInterval, and a
/// respawn that dies on its first attempt — e.g. NATS not yet reachable — is
/// retried), so the first instant after `bootstrap` is not judged.
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_RESPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// Consecutive polls a pid may show the wrong executable before that is a
/// failure (a fresh process reports its path a moment after it exists).
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_EXE_SETTLE_POLLS: u32 = 6;

/// bootout → bootstrap (retrying the transient I/O error) → wait for a pid →
/// require that pid to be running `expected`, the installed binary.
#[cfg(any(target_os = "macos", test))]
async fn restart_launchd_job<R: LaunchdRunner>(
    runner: &R,
    domain: &str,
    label: &str,
    plist: &Path,
    expected: &Path,
) -> Result<(), String> {
    let target = format!("{domain}/{label}");
    let plist_arg = plist
        .to_str()
        .ok_or_else(|| format!("{label}: plist path {} is not valid UTF-8", plist.display()))?;

    let out = runner
        .launchctl(&["bootout", &target])
        .await
        .map_err(|e| format!("{label}: {e}"))?;
    if !out.success() && !out.code.is_some_and(|c| LAUNCHCTL_NOT_LOADED.contains(&c)) {
        return Err(format!(
            "{label}: `launchctl bootout {target}` exited {:?}: {}",
            out.code,
            out.stderr.trim()
        ));
    }

    let mut attempt = 1;
    loop {
        let out = runner
            .launchctl(&["bootstrap", domain, plist_arg])
            .await
            .map_err(|e| format!("{label}: {e}"))?;
        if out.success() {
            break;
        }
        if out.code == Some(LAUNCHCTL_IO_ERROR) && attempt < BOOTSTRAP_ATTEMPTS {
            attempt += 1;
            runner.sleep(BOOTSTRAP_RETRY_INTERVAL).await;
            continue;
        }
        return Err(format!(
            "{label}: `launchctl bootstrap {domain} {}` exited {:?} after {attempt} attempt(s): {}",
            plist.display(),
            out.code,
            out.stderr.trim()
        ));
    }

    let expected_exe = canonical_or(expected);
    let polls = (LAUNCHD_RESPAWN_TIMEOUT.as_millis() / LAUNCHD_POLL_INTERVAL.as_millis()) as u32;
    let mut with_pid = 0;
    for poll in 0..polls {
        if poll > 0 {
            runner.sleep(LAUNCHD_POLL_INTERVAL).await;
        }
        let Some(pid) = launchd_pid(runner, &target).await else {
            with_pid = 0;
            continue;
        };
        with_pid += 1;
        let seen = match runner.exe_of(pid).await {
            Ok(exe) if canonical_or(&exe) == expected_exe => return Ok(()),
            Ok(exe) => exe.display().to_string(),
            Err(e) => e,
        };
        if with_pid >= LAUNCHD_EXE_SETTLE_POLLS {
            return Err(format!(
                "{label}: still not running the new binary after restart (pid {pid} runs {seen}, \
                 installed binary is {})",
                expected_exe.display()
            ));
        }
    }
    Err(format!(
        "{label}: still not running the new binary after restart (no pid after {}s)",
        LAUNCHD_RESPAWN_TIMEOUT.as_secs()
    ))
}

/// The job's pid, `None` while launchd has not started it.
#[cfg(any(target_os = "macos", test))]
async fn launchd_pid<R: LaunchdRunner>(runner: &R, target: &str) -> Option<u32> {
    let out = runner.launchctl(&["print", target]).await.ok()?;
    if !out.success() {
        return None;
    }
    parse_launchd_print_pid(&out.stdout)
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
    let path = canonical_or(&on_disk_path(path));
    binaries
        .iter()
        .any(|binary| canonical_or(&on_disk_path(binary)) == path)
}

fn matches_replaced(unit: &ServiceUnit, binaries: &[PathBuf]) -> bool {
    is_replaced(&unit.exec_path, binaries)
}

/// Drop the `" (deleted)"` marker the kernel appends for a file that was
/// unlinked while still mapped — which is exactly what `hs upgrade` does to its
/// own image before it records the binaries it replaced, and what a unit's
/// `ExecStart` path would read back as in that window. The identity we match on
/// is the path on disk.
///
/// Deliberately *not* applied to `/proc/<pid>/exe` when verifying a restart:
/// there the marker means the old inode is still running, which is a failure.
pub(crate) fn on_disk_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    match raw.strip_suffix(" (deleted)") {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
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

/// Program path (first `ProgramArguments` string) of a launchd plist.
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

/// A home-still launchd job and the binary its plist runs.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
struct LaunchdJob {
    label: String,
    active: bool,
    exec_path: PathBuf,
}

/// Resolve every home-still label in `launchctl list` output to its plist's
/// program. A label whose plist cannot be read or parsed is an error, not a
/// skipped unit: a running job we cannot identify might be running the old
/// binary.
#[cfg(any(target_os = "macos", test))]
fn launchd_jobs(
    list_stdout: &str,
    read_plist: impl Fn(&str) -> std::io::Result<String>,
) -> Result<Vec<LaunchdJob>> {
    let mut jobs = Vec::new();
    for (pid, label) in home_still_launchd_entries(list_stdout) {
        let plist = read_plist(&label)
            .map_err(|e| anyhow::anyhow!("{label}: cannot read its launchd plist: {e}"))?;
        let exec_path = parse_plist_program_path(&plist).ok_or_else(|| {
            anyhow::anyhow!("{label}: launchd plist has no parsable ProgramArguments[0]")
        })?;
        jobs.push(LaunchdJob {
            label,
            active: pid.is_some(),
            exec_path,
        });
    }
    Ok(jobs)
}

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_print_pid(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

// ── Distill index daemon ───────────────────────────────────────

/// Outcome of the index-daemon restart: `Ok(true)` restarted. A daemon that
/// was running and could not be brought back is an error — the old one was
/// already killed, so "nothing to do" would hide an outage.
async fn restart_index_daemon(reporter: &Arc<dyn Reporter>) -> Result<bool> {
    use crate::daemon::StopOutcome;

    // Only a PID that is really the index daemon is signaled; a PID file
    // naming anything else is stale and is removed.
    match crate::distill_cmd::stop_index_daemon().await? {
        StopOutcome::Stopped(pid) => {
            reporter.status("Restart", &format!("distill indexer (PID {pid})"));
            if !crate::distill_cmd::ensure_index_running().await? {
                bail!(
                    "the old indexer (PID {pid}) was stopped but a new one was not started: \
                     distill server binary missing or server unreachable"
                );
            }
            reporter.status("OK", "distill indexer restarted");
            Ok(true)
        }
        StopOutcome::Foreign(pid) => {
            reporter.warn(&format!(
                "distill-index.pid named PID {pid}, which is not the index daemon; \
                 removed the stale PID file and left that process alone"
            ));
            Ok(false)
        }
        StopOutcome::Dead(_) | StopOutcome::NoPidFile => Ok(false),
    }
}

// ── Docker compose containers ──────────────────────────────────

/// Decide whether one `compose restart` succeeded. The exit status is the
/// verdict; filtered stderr only supplies the explanation. A non-zero exit
/// with nothing left after filtering is still a failure.
fn classify_compose_restart(
    name: &str,
    success: bool,
    code: Option<i32>,
    stderr: &str,
) -> Result<(), String> {
    if success {
        return Ok(());
    }
    let errors = hs_common::compose::filter_compose_stderr(stderr);
    if errors.is_empty() {
        Err(format!(
            "{name} containers: `compose restart` exited {code:?}: {}",
            stderr.trim()
        ))
    } else {
        Err(format!("{name} containers: {}", errors.join("; ")))
    }
}

/// The compose stacks `hs` manages on this host, in restart/upgrade order:
/// the scribe stack only when `scribe.local_server` is on, the distill
/// (Qdrant) stack always — and only files that exist.
pub(crate) fn managed_compose_files(
    hidden: &Path,
    scribe_local_server: bool,
) -> Vec<(&'static str, PathBuf)> {
    let mut stacks: Vec<(&'static str, PathBuf)> = Vec::new();
    if scribe_local_server {
        stacks.push(("scribe", hidden.join("docker-compose.yml")));
    }
    stacks.push(("distill", hidden.join("docker-compose-distill.yml")));
    stacks.retain(|(_, p)| p.exists());
    stacks
}

/// The compose runtime for `files`. None installed is an error naming the
/// files, so a stale file is distinguishable from a missing runtime.
pub(crate) async fn compose_runtime_for(
    files: &[(&'static str, PathBuf)],
) -> Result<hs_common::compose::ComposeCmd> {
    let Some(compose) = hs_common::compose::ComposeCmd::detect().await else {
        bail!(
            "compose files exist ({}) but no compose runtime was found (docker compose, podman compose, docker-compose, podman-compose); install one, or remove the file if this host no longer runs those containers",
            files
                .iter()
                .map(|(_, p)| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    Ok(compose)
}

/// Restart the compose stacks that exist on this host. Returns the number
/// restarted and the per-stack failures; no compose files at all is `(0, [])`.
async fn restart_compose_services(reporter: &Arc<dyn Reporter>) -> Result<(u32, Vec<String>)> {
    let hidden = hs_common::hidden_dir()?;

    let scribe_cfg = hs_scribe::config::ScribeConfig::load()?;

    let active = managed_compose_files(&hidden, scribe_cfg.local_server);

    if active.is_empty() {
        return Ok((0, Vec::new()));
    }

    let compose = compose_runtime_for(&active).await?;

    let mut restarted = 0u32;
    let mut failures = Vec::new();
    for (name, path) in &active {
        let cf = path.to_string_lossy().to_string();
        reporter.status("Restart", &format!("{name} containers"));
        let verdict = match compose.run_capture(&["-f", &cf, "restart"]).await {
            Ok(o) => classify_compose_restart(
                name,
                o.status.success(),
                o.status.code(),
                &String::from_utf8_lossy(&o.stderr),
            ),
            Err(e) => Err(format!("{name} containers: could not run compose: {e:#}")),
        };
        match verdict {
            Ok(()) => {
                reporter.status("OK", &format!("{name} containers restarted"));
                restarted += 1;
            }
            Err(failure) => failures.push(failure),
        }
    }

    Ok((restarted, failures))
}

// ── Helpers ────────────────────────────────────────────────────

/// Whether a process's `/proc/<pid>/cgroup` content puts it inside
/// `unit_name`, which is the full name (`hs-serve-distill.service`) — the
/// suffix matters, since the cgroup path carries it too.
#[cfg(any(target_os = "linux", test))]
fn cgroup_is_under_unit(cgroup: &str, unit_name: &str) -> bool {
    cgroup.contains(&format!("/{unit_name}/")) || cgroup.contains(&format!("/{unit_name}\n"))
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
                if !cgroup_is_under_unit(&cgroup, unit_name) {
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

    const HS: &str = "/home/user/.local/bin/hs";

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
        let big = "{ path=/home/user/.local/bin/hs ; \
                   argv[]=/home/user/.local/bin/hs serve scribe --port 7435 ; \
                   ignore_errors=no ; start_time=[Wed 2026-09-16 06:48:59 CDT] ; \
                   stop_time=[n/a] ; pid=973 ; code=(null) ; status=0/0 }";
        assert_eq!(parse_exec_start_path(big), Some(PathBuf::from(HS)));

        let shell = "{ path=/bin/sh ; argv[]=/bin/sh -c 'exec hs serve mcp' ; ignore_errors=no ; }";
        assert_eq!(parse_exec_start_path(shell), Some(PathBuf::from("/bin/sh")));

        assert_eq!(parse_exec_start_path(""), None);
    }

    #[test]
    fn parse_exec_start_argv_and_port() {
        let big = "{ path=/home/user/.local/bin/hs ; \
                   argv[]=/home/user/.local/bin/hs serve scribe --port 7435 ; \
                   ignore_errors=no ; }";
        let argv = parse_exec_start_argv(big).expect("argv");
        assert_eq!(argv, "/home/user/.local/bin/hs serve scribe --port 7435");
        assert_eq!(parse_serve_port(&argv), Some(7435));

        // Someone else's server: not ours to probe.
        assert_eq!(parse_serve_port("/usr/bin/vllm serve --port 8081"), None);
        // The watcher daemons bind nothing.
        assert_eq!(
            parse_serve_port("/home/user/.local/bin/hs distill watch-events"),
            None
        );
    }

    #[test]
    fn matches_replaced_requires_exact_binary() {
        let installed = PathBuf::from(HS);
        let dev = PathBuf::from("/home/user/home-still/target/release/hs");

        assert!(matches_replaced(
            &unit(UnitScope::System, "hs-serve-mcp.service", HS),
            std::slice::from_ref(&installed)
        ));
        // The dev build in target/release must not be matched by substring.
        assert!(!matches_replaced(
            &unit(
                UnitScope::System,
                "hs-distill-watch.service",
                "/home/user/home-still/target/release/hs"
            ),
            &[installed]
        ));
        assert!(matches_replaced(
            &unit(
                UnitScope::System,
                "hs-distill-watch.service",
                "/home/user/home-still/target/release/hs"
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
        <string>/Users/user/.local/bin/hs</string>
        <string>serve</string>
        <string>scribe</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#;
        assert_eq!(
            parse_plist_program_path(plist),
            Some(PathBuf::from("/Users/user/.local/bin/hs"))
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
    fn cgroup_match_uses_the_full_unit_name() {
        // Real cgroup lines from a production host.
        assert!(cgroup_is_under_unit(
            "0::/home.slice/home-still.slice/hs-serve-distill.service\n",
            "hs-serve-distill.service"
        ));
        assert!(cgroup_is_under_unit(
            "0::/system.slice/hs-serve-mcp.service\n",
            "hs-serve-mcp.service"
        ));
        assert!(cgroup_is_under_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/hs-scribe-watch-events.service\n",
            "hs-scribe-watch-events.service"
        ));

        // A different unit, and a sibling whose name only shares a prefix.
        assert!(!cgroup_is_under_unit(
            "0::/system.slice/hs-serve-mcp.service\n",
            "hs-serve-distill.service"
        ));
        assert!(!cgroup_is_under_unit(
            "0::/system.slice/hs-serve-mcp.service\n",
            "hs-serve-mcp"
        ));
    }

    /// The `hs upgrade` failure that shipped in rc.354: it swaps its own image
    /// first, so every recorded path read back as `… (deleted)` and no unit
    /// matched — "No running services found to restart" with units running the
    /// deleted binary.
    #[cfg(unix)]
    #[test]
    fn deleted_marker_still_identifies_the_binary() {
        let dir = std::env::temp_dir().join(format!("hs-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let real = dir.join("hs.personal-feature");
        std::fs::write(&real, b"bin").expect("write image");
        let link = dir.join("hs");
        std::os::unix::fs::symlink("hs.personal-feature", &link).expect("symlink");

        let recorded = vec![PathBuf::from(format!("{} (deleted)", real.display()))];
        assert!(matches_replaced(
            &unit(
                UnitScope::System,
                "hs-serve-mcp.service",
                link.to_str().expect("utf8")
            ),
            &recorded
        ));
        assert!(!matches_replaced(
            &unit(UnitScope::System, "other.service", "/bin/true"),
            &recorded
        ));

        std::fs::remove_dir_all(&dir).ok();
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

    // ── RA-65: discovery failures are errors ──────────────────

    #[cfg(unix)]
    fn output(code: i32, stdout: &str, stderr: &str) -> std::io::Result<std::process::Output> {
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[cfg(unix)]
    #[test]
    fn list_units_nonzero_exit_is_an_error() {
        let err = interpret_list_units(output(1, "", "Failed to connect to bus"), true)
            .expect_err("a failing systemctl must not read as zero units");
        let msg = format!("{err:#}");
        assert!(msg.contains("systemctl --user"), "{msg}");
        assert!(msg.contains("Failed to connect to bus"), "{msg}");
    }

    #[test]
    fn list_units_without_systemctl_is_zero_units() {
        let missing = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(
            interpret_list_units(missing, false).unwrap(),
            Vec::<String>::new()
        );

        let denied = Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(interpret_list_units(denied, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn list_units_extracts_service_names_including_failed_rows() {
        let listed = "  hs-serve-mcp.service      loaded active running Home-Still mcp\n\
                      ● hs-serve-scribe.service   loaded failed failed  Home-Still scribe\n";
        assert_eq!(
            interpret_list_units(output(0, listed, ""), false).unwrap(),
            vec!["hs-serve-mcp.service", "hs-serve-scribe.service"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_unit_parses_a_listed_unit() {
        let show = format!(
            "ExecStart={{ path={HS} ; argv[]={HS} serve mcp --port 7445 ; ignore_errors=no }}\n\
             ActiveState=active\nUnitFileState=enabled\nType=simple\n"
        );
        let unit = interpret_show_unit(false, "hs-serve-mcp.service", output(0, &show, ""))
            .unwrap()
            .expect("unit");
        assert_eq!(unit.exec_path, PathBuf::from(HS));
        assert!(unit.active && unit.enabled && !unit.oneshot);
        assert_eq!(unit.scope, UnitScope::System);
    }

    #[cfg(unix)]
    #[test]
    fn show_unit_empty_exec_start_is_skipped_not_failed() {
        let show = "ExecStart=\nActiveState=inactive\nUnitFileState=static\nType=oneshot\n";
        assert_eq!(
            interpret_show_unit(true, "hs-x.service", output(0, show, "")).unwrap(),
            None
        );
    }

    /// Found on the rc.362 rollout: a failed transient unit whose file is
    /// gone (`not-found`) and a masked unit have no `ExecStart` property, and
    /// one of them aborted the whole restart phase on two hosts.
    #[cfg(unix)]
    #[test]
    fn show_unit_not_loaded_units_are_skipped_not_failed() {
        for load_state in ["not-found", "masked"] {
            let show = format!("LoadState={load_state}\nActiveState=failed\n");
            assert_eq!(
                interpret_show_unit(false, "hs-gone.service", output(0, &show, "")).unwrap(),
                None,
                "{load_state}"
            );
        }
    }

    /// The rc.363 rollout bug: the not-loaded check above was right, but
    /// `LoadState` was never requested, so real `systemctl show` output never
    /// carried it and a masked unit still aborted the restart phase.
    #[cfg(unix)]
    #[test]
    fn show_unit_args_request_the_load_state() {
        let args = show_unit_args("hs-x.service");
        assert_eq!(args[..2], ["show", "hs-x.service"]);
        assert!(
            args.windows(2).any(|w| w == ["-p", "LoadState"]),
            "{args:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn show_unit_malformed_or_failed_output_is_an_error() {
        // Listed unit whose show output lacks ExecStart entirely.
        let e = interpret_show_unit(false, "hs-a.service", output(0, "ActiveState=active\n", ""))
            .unwrap_err();
        assert!(format!("{e:#}").contains("hs-a.service"));
        // ExecStart present but no parsable path (previously: unit silently skipped).
        let e = interpret_show_unit(
            false,
            "hs-b.service",
            output(0, "ExecStart=garbage\nActiveState=active\n", ""),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("hs-b.service"));
        // Missing ActiveState.
        let show = format!("ExecStart={{ path={HS} ; argv[]={HS} }}\n");
        assert!(interpret_show_unit(false, "hs-c.service", output(0, &show, "")).is_err());
        // systemctl show itself failed.
        let e = interpret_show_unit(false, "hs-d.service", output(1, "", "boom")).unwrap_err();
        assert!(format!("{e:#}").contains("boom"));
    }

    #[test]
    fn launchd_unreadable_or_unparsable_plist_is_an_error() {
        let listed = "PID\tStatus\tLabel\n1340\t0\tcom.home-still.scribe\n7\t0\tcom.apple.other\n";
        let good =
            "<key>ProgramArguments</key><array><string>/Users/user/.local/bin/hs</string></array>";

        let jobs = launchd_jobs(listed, |_| Ok(good.to_string())).unwrap();
        assert_eq!(
            jobs,
            vec![LaunchdJob {
                label: "com.home-still.scribe".to_string(),
                active: true,
                exec_path: PathBuf::from("/Users/user/.local/bin/hs"),
            }]
        );

        let missing = launchd_jobs(listed, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert!(format!("{missing:#}").contains("com.home-still.scribe"));

        let garbled = launchd_jobs(listed, |_| Ok("<plist></plist>".to_string())).unwrap_err();
        assert!(format!("{garbled:#}").contains("com.home-still.scribe"));
    }

    // ── launchd restart: bootout → bootstrap → pid → executable ─

    const DOMAIN: &str = "gui/501";
    const LABEL: &str = "com.home-still.scribe";
    const PLIST: &str = "/Users/u/Library/LaunchAgents/com.home-still.scribe.plist";
    const INSTALLED: &str = "/Users/u/.local/bin/hs";

    /// A scripted launchd: each list is consumed in order and its last entry
    /// repeats.
    struct FakeLaunchd {
        bootout: i32,
        bootstrap: Vec<i32>,
        pids: Vec<Option<u32>>,
        exes: Vec<Result<&'static str, &'static str>>,
        calls: std::cell::RefCell<Vec<String>>,
        sleeps: std::cell::Cell<u32>,
        prints: std::cell::Cell<usize>,
        exe_reads: std::cell::Cell<usize>,
    }

    impl FakeLaunchd {
        fn new(
            bootout: i32,
            bootstrap: &[i32],
            pids: &[Option<u32>],
            exes: &[Result<&'static str, &'static str>],
        ) -> Self {
            Self {
                bootout,
                bootstrap: bootstrap.to_vec(),
                pids: pids.to_vec(),
                exes: exes.to_vec(),
                calls: Default::default(),
                sleeps: Default::default(),
                prints: Default::default(),
                exe_reads: Default::default(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn bootstraps(&self) -> usize {
            self.calls()
                .iter()
                .filter(|c| c.starts_with("bootstrap"))
                .count()
        }

        fn exit(code: i32, stdout: String) -> LaunchctlExit {
            LaunchctlExit {
                code: Some(code),
                stdout,
                stderr: format!("launchctl said {code}"),
            }
        }
    }

    fn nth<T: Clone>(items: &[T], n: usize) -> T {
        items[n.min(items.len() - 1)].clone()
    }

    impl LaunchdRunner for FakeLaunchd {
        async fn launchctl(&self, args: &[&str]) -> Result<LaunchctlExit, String> {
            self.calls.borrow_mut().push(args.join(" "));
            Ok(match args[0] {
                "bootout" => Self::exit(self.bootout, String::new()),
                "bootstrap" => {
                    let n = self
                        .calls()
                        .iter()
                        .filter(|c| c.starts_with("bootstrap"))
                        .count();
                    Self::exit(nth(&self.bootstrap, n - 1), String::new())
                }
                "print" => {
                    let n = self.prints.get();
                    self.prints.set(n + 1);
                    match nth(&self.pids, n) {
                        Some(pid) => Self::exit(0, format!("state = running\n\tpid = {pid}\n")),
                        None => Self::exit(0, "state = waiting\n".to_string()),
                    }
                }
                other => panic!("unexpected launchctl {other}"),
            })
        }

        async fn exe_of(&self, _pid: u32) -> Result<PathBuf, String> {
            let n = self.exe_reads.get();
            self.exe_reads.set(n + 1);
            nth(&self.exes, n)
                .map(PathBuf::from)
                .map_err(str::to_string)
        }

        async fn sleep(&self, _duration: std::time::Duration) {
            self.sleeps.set(self.sleeps.get() + 1);
        }
    }

    async fn restart(fake: &FakeLaunchd) -> Result<(), String> {
        restart_launchd_job(fake, DOMAIN, LABEL, Path::new(PLIST), Path::new(INSTALLED)).await
    }

    #[tokio::test]
    async fn a_job_is_booted_out_and_bootstrapped_from_its_plist() {
        let fake = FakeLaunchd::new(0, &[0], &[Some(4242)], &[Ok(INSTALLED)]);
        restart(&fake).await.unwrap();
        assert_eq!(
            fake.calls(),
            [
                format!("bootout {DOMAIN}/{LABEL}"),
                format!("bootstrap {DOMAIN} {PLIST}"),
                format!("print {DOMAIN}/{LABEL}"),
            ]
        );
    }

    #[tokio::test]
    async fn the_transient_bootstrap_io_error_is_retried_until_it_succeeds() {
        let fake = FakeLaunchd::new(0, &[5, 5, 0], &[Some(4242)], &[Ok(INSTALLED)]);
        restart(&fake).await.unwrap();
        assert_eq!(fake.bootstraps(), 3);
        assert_eq!(fake.sleeps.get(), 2);
    }

    #[tokio::test]
    async fn a_persistent_bootstrap_io_error_fails_after_five_attempts() {
        let fake = FakeLaunchd::new(0, &[5], &[Some(4242)], &[Ok(INSTALLED)]);
        let err = restart(&fake).await.unwrap_err();
        assert_eq!(fake.bootstraps(), 5);
        assert!(
            err.contains("bootstrap") && err.contains("5 attempt") && err.contains(PLIST),
            "{err}"
        );
    }

    #[tokio::test]
    async fn any_other_bootstrap_failure_is_not_retried() {
        let fake = FakeLaunchd::new(0, &[1], &[Some(4242)], &[Ok(INSTALLED)]);
        let err = restart(&fake).await.unwrap_err();
        assert_eq!(fake.bootstraps(), 1);
        assert!(err.contains("exited Some(1)"), "{err}");
    }

    #[tokio::test]
    async fn bootout_of_a_job_that_was_not_loaded_is_not_a_failure() {
        for code in [3, 113] {
            let fake = FakeLaunchd::new(code, &[0], &[Some(4242)], &[Ok(INSTALLED)]);
            restart(&fake).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_bootout_failure_stops_before_bootstrap() {
        let fake = FakeLaunchd::new(1, &[0], &[Some(4242)], &[Ok(INSTALLED)]);
        let err = restart(&fake).await.unwrap_err();
        assert_eq!(fake.bootstraps(), 0);
        assert!(err.contains("bootout"), "{err}");
    }

    #[tokio::test]
    async fn a_pid_that_appears_after_respawns_is_waited_for() {
        let fake = FakeLaunchd::new(0, &[0], &[None, None, None, Some(4242)], &[Ok(INSTALLED)]);
        restart(&fake).await.unwrap();
        assert_eq!(fake.sleeps.get(), 3);
    }

    #[tokio::test]
    async fn a_job_that_never_gets_a_pid_fails() {
        let fake = FakeLaunchd::new(0, &[0], &[None], &[Ok(INSTALLED)]);
        let err = restart(&fake).await.unwrap_err();
        assert!(err.contains("no pid after 45s"), "{err}");
    }

    #[tokio::test]
    async fn a_pid_running_another_binary_names_both_paths() {
        let fake = FakeLaunchd::new(0, &[0], &[Some(4242)], &[Ok("/Users/u/old/bin/hs")]);
        let err = restart(&fake).await.unwrap_err();
        assert!(
            err.contains("/Users/u/old/bin/hs") && err.contains(INSTALLED),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_executable_that_is_only_briefly_unreadable_is_waited_for() {
        let fake = FakeLaunchd::new(
            0,
            &[0],
            &[Some(4242)],
            &[Err("no such process"), Ok(INSTALLED)],
        );
        restart(&fake).await.unwrap();
    }

    // ── RA-66: compose exit status is the verdict ─────────────

    #[test]
    fn compose_nonzero_with_empty_stderr_is_a_failure() {
        assert!(classify_compose_restart("distill", true, Some(0), "").is_ok());

        let err = classify_compose_restart("distill", false, Some(1), "").unwrap_err();
        assert!(err.contains("distill"), "{err}");

        // Stderr that is pure podman banner noise filters to nothing: still a failure.
        let noise = ">>>> Executing external compose provider \"podman-compose\"\n";
        assert!(classify_compose_restart("distill", false, Some(1), noise).is_err());

        let err = classify_compose_restart("distill", false, Some(1), "Error: no such service")
            .unwrap_err();
        assert!(err.contains("no such service"), "{err}");
    }

    #[test]
    fn managed_compose_files_follow_local_server_and_existence() {
        fn names(files: Vec<(&'static str, PathBuf)>) -> Vec<&'static str> {
            files.into_iter().map(|(n, _)| n).collect()
        }

        let dir = tempfile::tempdir().unwrap();
        let scribe = dir.path().join("docker-compose.yml");
        let distill = dir.path().join("docker-compose-distill.yml");
        std::fs::write(&scribe, "services: {}\n").unwrap();
        std::fs::write(&distill, "services: {}\n").unwrap();

        // A scribe file on a host with `local_server: false` is stale, not ours.
        assert_eq!(names(managed_compose_files(dir.path(), false)), ["distill"]);
        assert_eq!(
            names(managed_compose_files(dir.path(), true)),
            ["scribe", "distill"]
        );

        std::fs::remove_file(&distill).unwrap();
        assert_eq!(names(managed_compose_files(dir.path(), true)), ["scribe"]);

        std::fs::remove_file(&scribe).unwrap();
        assert!(managed_compose_files(dir.path(), true).is_empty());
    }
}
