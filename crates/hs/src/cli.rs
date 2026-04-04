use clap::{Parser, Subcommand};

/// hs - home-still research
#[derive(Parser, Debug)]
#[command(name = "hs", version, about, long_about = None, after_help = "\
Examples:                                                                   
hs paper search \"transformers\"                                          
hs paper download --doi \"10.48550/arXiv.2301.00001\"                     
hs config init")]
pub struct Cli {
    #[command(flatten)]
    pub global: hs_common::global_args::GlobalArgs,

    #[command(subcommand)]
    pub command: TopCmd,
}

#[derive(Subcommand, Debug)]
pub enum TopCmd {
    /// PDF-to-markdown via scribe server
    Scribe {
        #[command(subcommand)]
        command: super::scribe_cmd::ScribeCmd,
    },
    /// Academic paper search, lookup, and download
    #[command(after_help = "\
  Examples:
    hs paper search \"transformer attention\"                                 
    hs paper search --type author \"Hinton\" -n 5                             
    hs paper download \"neural nets\" -n 25                                   
    hs paper get --doi \"10.48550/arXiv.2301.00001\"")]
    Paper {
        #[command(subcommand)]
        command: paper::cli::PaperCmd,
    },
    /// Distill markdown into vector embeddings for semantic search
    Distill {
        #[command(subcommand)]
        command: hs_distill::cli::DistillCmd,
    },
    /// Live dashboard — pipeline health, services, recent activity
    Status,
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
