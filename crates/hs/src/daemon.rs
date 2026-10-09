use anyhow::{Context, Result};
use std::path::Path;

/// Read PID from a PID file. Returns None if file doesn't exist or is corrupt.
/// A PID outside `1..=i32::MAX` is corrupt too: callers hand it to `kill(2)`
/// as an `i32`, where 0 and negative values address whole process groups
/// instead of one process.
pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| (1..=i32::MAX as u32).contains(pid))
}

/// Check if a process with the given PID is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    false // daemon mode not supported on Windows
}

/// What a live process is running: its executable's file name and its
/// arguments (everything after `argv[0]`). Only built where processes can be
/// inspected (Unix).
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct ProcessInfo {
    exe_name: String,
    args: Vec<String>,
}

/// File name of an executable path as the OS reports it; `/proc/<pid>/exe`
/// of a binary replaced on disk (an upgrade renames the new file over the
/// old one) reads `<path> (deleted)`.
fn exe_file_name(exe: &str) -> String {
    crate::restart_cmd::on_disk_path(Path::new(exe))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The running `hs`'s path on disk. Linux reports a replaced image as
/// `<path> (deleted)`, which is not a path that can be spawned; after
/// `hs upgrade` swapped the binary, the file at `<path>` is the new `hs`.
pub fn current_exe_on_disk() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("cannot find the running hs executable")?;
    Ok(crate::restart_cmd::on_disk_path(&exe))
}

/// File name of the running `hs`, which is what a daemon it spawned (via
/// [`current_exe_on_disk`]) runs as.
pub fn current_exe_name() -> Result<String> {
    Ok(exe_file_name(&current_exe_on_disk()?.to_string_lossy()))
}

/// Whether `info` is `exe_name` run with `args` as a contiguous run of its
/// arguments (`[]` matches any argument list).
fn info_matches(info: &ProcessInfo, exe_name: &str, args: &[&str]) -> bool {
    info.exe_name == exe_name
        && (args.is_empty()
            || info
                .args
                .windows(args.len())
                .any(|w| w.iter().map(String::as_str).eq(args.iter().copied())))
}

/// Split a `/proc/<pid>/cmdline` buffer (NUL-terminated `argv`) into the
/// arguments after `argv[0]`.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn args_from_cmdline(cmdline: &[u8]) -> Vec<String> {
    cmdline
        .split(|b| *b == 0)
        .skip(1)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    // A zombie has no exe link and an empty cmdline, so it reads as "not ours".
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(ProcessInfo {
        exe_name: exe_file_name(&exe.to_string_lossy()),
        args: args_from_cmdline(&cmdline),
    })
}

/// Other Unixes (macOS): ask `ps`. `comm=` is the executable's path, `args=`
/// its full command line; a zombie's command line is `(name)`, which carries
/// no arguments and so never matches.
#[cfg(all(unix, not(target_os = "linux")))]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let ps = |field: &str| -> Option<String> {
        let out = std::process::Command::new("ps")
            .args(["-o", field, "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    };
    let comm = ps("comm=")?;
    let args = ps("args=")?;
    Some(ProcessInfo {
        exe_name: exe_file_name(&comm),
        args: args
            .split_whitespace()
            .skip(1)
            .map(str::to_string)
            .collect(),
    })
}

#[cfg(not(unix))]
fn process_info(_pid: u32) -> Option<ProcessInfo> {
    None
}

/// Is `pid` a live process running the executable `exe_name` with `args` as
/// a contiguous run of its arguments? A PID file outlives its process, and
/// the kernel recycles PIDs, so "the PID is alive" does not mean "the
/// process I started is alive". Anything that cannot be inspected (another
/// user's process, a zombie, a vanished PID) is not a match.
pub fn process_is(pid: u32, exe_name: &str, args: &[&str]) -> bool {
    process_info(pid).is_some_and(|info| info_matches(&info, exe_name, args))
}

/// The result of [`stop_pid_file_process`].
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// No PID file, or one that holds no valid PID.
    NoPidFile,
    /// The PID file named a PID that is no longer running; the file was removed.
    Dead(u32),
    /// The PID file named a live process that is not the expected one (a
    /// recycled PID); the file was removed and the process left alone.
    Foreign(u32),
    /// The expected process was terminated and the PID file removed.
    Stopped(u32),
}

/// Stop the process recorded in `pid_path`, but only after confirming it is
/// `exe_name` run with `args` ([`process_is`]). Anything else holding that
/// PID is never signaled: the PID file is stale and is removed. SIGTERM, up
/// to five seconds of grace, then SIGKILL.
pub async fn stop_pid_file_process(pid_path: &Path, exe_name: &str, args: &[&str]) -> StopOutcome {
    let Some(pid) = read_pid(pid_path) else {
        return StopOutcome::NoPidFile;
    };
    if !is_process_alive(pid) {
        remove_pid_file(pid_path);
        return StopOutcome::Dead(pid);
    }
    if !process_is(pid, exe_name, args) {
        remove_pid_file(pid_path);
        return StopOutcome::Foreign(pid);
    }
    #[cfg(unix)]
    {
        // SAFETY: `pid` is in `1..=i32::MAX` (`read_pid`), so this signals
        // exactly one process.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..50 {
            if !is_process_alive(pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if is_process_alive(pid) {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    remove_pid_file(pid_path);
    StopOutcome::Stopped(pid)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn own_exe_name() -> String {
        exe_file_name(&std::env::current_exe().unwrap().to_string_lossy())
    }

    #[test]
    fn deleted_suffix_is_not_part_of_the_exe_name() {
        assert_eq!(exe_file_name("/home/u/.local/bin/hs (deleted)"), "hs");
        assert_eq!(exe_file_name("/home/u/.local/bin/hs"), "hs");
    }

    #[test]
    fn cmdline_args_skip_argv0_and_empty_fields() {
        assert_eq!(
            args_from_cmdline(b"/bin/hs\0distill\0index\0--daemon-child\0"),
            ["distill", "index", "--daemon-child"]
        );
        assert!(args_from_cmdline(b"").is_empty());
    }

    #[test]
    fn matcher_needs_the_name_and_the_contiguous_args() {
        let info = ProcessInfo {
            exe_name: "hs".into(),
            args: ["distill", "index", "--daemon-child", "--force"]
                .map(String::from)
                .into(),
        };
        assert!(info_matches(&info, "hs", &[]));
        assert!(info_matches(
            &info,
            "hs",
            &["distill", "index", "--daemon-child"]
        ));
        assert!(!info_matches(&info, "hs", &["index", "distill"]));
        assert!(!info_matches(&info, "hs", &["distill", "--daemon-child"]));
        assert!(!info_matches(&info, "other", &[]));
    }

    #[test]
    fn this_process_is_itself_and_nothing_else() {
        let me = std::process::id();
        assert!(process_is(me, &own_exe_name(), &[]));
        assert!(!process_is(me, "definitely-not-this-binary", &[]));
    }

    #[tokio::test]
    async fn a_pid_file_naming_another_process_is_stale_and_not_signaled() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("x.pid");
        // This very process holds the PID: alive, but not "not-the-server".
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        let outcome = stop_pid_file_process(&pid_path, "not-the-server", &[]).await;
        assert_eq!(outcome, StopOutcome::Foreign(std::process::id()));
        assert!(!pid_path.exists());

        assert_eq!(
            stop_pid_file_process(&pid_path, "not-the-server", &[]).await,
            StopOutcome::NoPidFile
        );
    }
}

/// Write PID to file.
pub fn write_pid_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string()).context("Failed to write PID file")
}

/// Remove PID file.
pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}
