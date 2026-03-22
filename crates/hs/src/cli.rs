use clap::{Parser, Subcommand};

/// hs - home-still research
#[derive(Parser, Debug)]
#[command(name = "hs", version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global: hs_style::global_args::GlobalArgs,

    #[command(subcommand)]
    pub command: TopCmd,
}

#[derive(Subcommand, Debug)]
pub enum TopCmd {
    /// Academic paper search, lookup, and download
    Paper {
        #[command(subcommand)]
        command: paper::cli::PaperCmd,
    },
    /// View and manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Generate default config at ~/.home-still/config.yaml
    Init {
        /// Overwrite existing config file
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved configuration
    Show,
    /// Print the config file path
    Path,
}
