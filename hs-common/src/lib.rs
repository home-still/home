pub mod exit_codes;
pub mod mode;
pub mod pipe_reporter;
pub mod reporter;

/// Relative path from $HOME to the config file.
pub const CONFIG_REL_PATH: &str = ".home-still/config.yaml";

/// Hidden directory for config, cache, models (relative to $HOME).
pub const HIDDEN_DIR: &str = ".home-still";

/// Visible project directory for papers, markdown (relative to $HOME).
pub const PROJECT_DIR_DEFAULT: &str = "home-still";

/// Resolve the project directory from config (home.project_dir) or default ~/home-still.
/// This reads the config file directly to avoid heavy YAML parser dependencies.
pub fn resolve_project_dir() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(CONFIG_REL_PATH);

    // Try to read project_dir from config file
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        // Simple line-by-line scan for project_dir
        let mut in_home_section = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            // Track YAML sections: top-level keys have no leading whitespace
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_home_section = trimmed.starts_with("home:");
            }
            if in_home_section {
                if let Some(val) = trimmed.strip_prefix("project_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        if let Some(rest) = val.strip_prefix("~/") {
                            return home.join(rest);
                        }
                        return std::path::PathBuf::from(val);
                    }
                }
            }
        }
    }

    // Default: ~/home-still
    home.join(PROJECT_DIR_DEFAULT)
}

/// Resolve the log directory from config (home.log_dir) or default {project_dir}/logs.
pub fn resolve_log_dir() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_path = home.join(CONFIG_REL_PATH);

    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        let mut in_home_section = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_home_section = trimmed.starts_with("home:");
            }
            if in_home_section {
                if let Some(val) = trimmed.strip_prefix("log_dir:") {
                    let val = val.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        if let Some(rest) = val.strip_prefix("~/") {
                            return home.join(rest);
                        }
                        return std::path::PathBuf::from(val);
                    }
                }
            }
        }
    }

    resolve_project_dir().join("logs")
}

#[cfg(feature = "cli")]
pub mod global_args;
#[cfg(feature = "cli")]
pub mod styles;
#[cfg(feature = "cli")]
pub mod tty_reporter;

#[cfg(feature = "service")]
pub mod service;

#[cfg(feature = "catalog")]
pub mod catalog;

#[cfg(feature = "compose")]
pub mod compose;

#[cfg(feature = "auth")]
pub mod auth;

#[cfg(all(feature = "compose", feature = "service"))]
pub mod gpu_priority;
