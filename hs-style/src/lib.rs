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
            // Track YAML sections (top-level keys ending with :)
            if !trimmed.starts_with('#') && !trimmed.is_empty() {
                if !trimmed.starts_with(' ') && !trimmed.starts_with('\t') {
                    in_home_section = trimmed.starts_with("home:");
                }
                if in_home_section {
                    if let Some(val) = trimmed.strip_prefix("project_dir:") {
                        let val = val.trim().trim_matches('"').trim_matches('\'');
                        if !val.is_empty() {
                            // Expand ~ to home dir
                            if let Some(rest) = val.strip_prefix("~/") {
                                return home.join(rest);
                            }
                            return std::path::PathBuf::from(val);
                        }
                    }
                }
            }
        }
    }

    // Default: ~/home-still
    home.join(PROJECT_DIR_DEFAULT)
}

#[cfg(feature = "cli")]
pub mod global_args;
#[cfg(feature = "cli")]
pub mod styles;
#[cfg(feature = "cli")]
pub mod tty_reporter;
