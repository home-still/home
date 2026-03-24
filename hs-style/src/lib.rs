pub mod exit_codes;
pub mod mode;
pub mod pipe_reporter;
pub mod reporter;

/// Relative path from $HOME to the config file.                               
pub const CONFIG_REL_PATH: &str = ".home-still/config.yaml";

#[cfg(feature = "cli")]
pub mod global_args;
#[cfg(feature = "cli")]
pub mod styles;
#[cfg(feature = "cli")]
pub mod tty_reporter;
