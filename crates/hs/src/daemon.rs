use anyhow::{Context, Result};
use std::path::Path;

/// Read PID from a PID file. Returns None if file doesn't exist or is corrupt.
pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
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
