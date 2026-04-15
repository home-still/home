//! GPU priority coordination between scribe and distill.
//!
//! Distill checks scribe's status file before each embedding.
//! If scribe has work queued, distill yields and polls until scribe is idle.

use std::path::PathBuf;

const SCRIBE_STATUS_FILE: &str = ".scribe-watch-status.json";
const POLL_INTERVAL_SECS: u64 = 5;

/// Resolve the scribe status file path from config or default.
fn scribe_status_path() -> PathBuf {
    // Try to read scribe output_dir from config, fall back to default
    let project = crate::resolve_project_dir();
    let output_dir = project.join("markdown");

    // If hs-scribe config is available, use its output_dir
    // But we can't depend on hs-scribe from hs-common, so we read the config directly
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(crate::CONFIG_REL_PATH);

    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        let mut in_scribe_section = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_scribe_section = trimmed.starts_with("scribe:");
            }
            if in_scribe_section {
                if let Some(val) = trimmed.strip_prefix("output_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        let dir = if let Some(rest) = val.strip_prefix("~/") {
                            home.join(rest)
                        } else {
                            PathBuf::from(val)
                        };
                        return dir.join(SCRIBE_STATUS_FILE);
                    }
                }
            }
        }
    }

    output_dir.join(SCRIBE_STATUS_FILE)
}

/// Check if scribe has active work (processing or queued PDFs).
/// Returns false if the status file doesn't exist or is unreadable.
pub fn scribe_is_active() -> bool {
    let path = scribe_status_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false, // No status file = scribe not running
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let processing = value["processing"].as_u64().unwrap_or(0);
    let queued = value["queued"].as_u64().unwrap_or(0);

    processing > 0 || queued > 0
}

/// Wait until scribe is idle (no processing or queued work).
/// Polls every 5 seconds. Returns immediately if scribe is not running.
pub async fn wait_for_scribe_idle() {
    loop {
        if !scribe_is_active() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}
