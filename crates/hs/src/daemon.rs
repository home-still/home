use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Get the PID file path for a given watch directory.
/// Uses a hash of the canonical watch dir to generate a unique filename.
pub fn pid_file_path(watch_dir: &Path) -> PathBuf {
    let canonical = watch_dir
        .canonicalize()
        .unwrap_or_else(|_| watch_dir.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_style::HIDDEN_DIR)
        .join(format!("scribe-watch-{hash}.pid"))
}

/// Read PID from a PID file. Returns None if file doesn't exist or is corrupt.
pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Check if a process with the given PID is alive.
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
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

/// Check if we can start a new daemon. Returns Ok(()) if no daemon is running,
/// or Err with the existing PID if one is already running.
pub fn acquire_instance_lock(watch_dir: &Path) -> Result<(), u32> {
    let pid_path = pid_file_path(watch_dir);
    if let Some(pid) = read_pid(&pid_path) {
        if is_process_alive(pid) {
            return Err(pid); // already running
        }
        // Stale PID file — process is dead, clean up
        remove_pid_file(&pid_path);
    }
    Ok(())
}

/// Spawn the daemon as a detached child process.
/// Re-execs the current binary with internal `--daemon-child` flag.
pub fn spawn_daemon(dir: Option<&str>, outdir: Option<&str>, server: Option<&str>) -> Result<u32> {
    let exe = std::env::current_exe().context("Cannot find current executable")?;

    let mut args = vec![
        "scribe".to_string(),
        "watch".to_string(),
        "--daemon-child".to_string(),
    ];
    if let Some(d) = dir {
        args.push("--dir".to_string());
        args.push(d.to_string());
    }
    if let Some(o) = outdir {
        args.push("--outdir".to_string());
        args.push(o.to_string());
    }
    if let Some(s) = server {
        args.push("--server".to_string());
        args.push(s.to_string());
    }

    let log_path = dirs::home_dir()
        .unwrap_or_default()
        .join(hs_style::HIDDEN_DIR)
        .join("scribe-watch.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(Path::new(".")));

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Cannot open daemon log file")?;
    let log_err = log_file
        .try_clone()
        .context("Cannot clone log file handle")?;

    let child = std::process::Command::new(exe)
        .args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn daemon")?;

    Ok(child.id())
}

/// Send SIGTERM to the daemon, wait for exit, fallback to SIGKILL.
pub fn stop_daemon(watch_dir: &Path) -> Result<Option<u32>> {
    let pid_path = pid_file_path(watch_dir);
    let pid = match read_pid(&pid_path) {
        Some(pid) if is_process_alive(pid) => pid,
        Some(_) => {
            // Stale PID file
            remove_pid_file(&pid_path);
            return Ok(None);
        }
        None => return Ok(None),
    };

    // Send SIGTERM
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    // Wait up to 5 seconds for process to exit
    for _ in 0..50 {
        if !is_process_alive(pid) {
            remove_pid_file(&pid_path);
            return Ok(Some(pid));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Force kill
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    remove_pid_file(&pid_path);
    Ok(Some(pid))
}

/// Set up the daemon child process: write PID file.
/// Called when `--daemon-child` flag is present.
pub fn setup_daemon_child(watch_dir: &Path) -> Result<()> {
    let pid_path = pid_file_path(watch_dir);
    write_pid_file(&pid_path)?;
    Ok(())
}

/// Clean up on daemon exit: remove PID file.
pub fn cleanup_daemon(watch_dir: &Path) {
    let pid_path = pid_file_path(watch_dir);
    remove_pid_file(&pid_path);
}
