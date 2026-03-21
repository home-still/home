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
}
