use clap::{Parser, Subcommand};

/// hs - home-still research
#[derive(Parser, Debug)]
#[command(name = "hs", version = env!("HS_VERSION"), about, long_about = None, after_help = "\
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
    /// Restart all running services (system services, daemons, containers)
    Restart,
    /// Check for updates and upgrade hs + managed services
    Upgrade {
        /// Only check if an update is available; do not install
        #[arg(long)]
        check: bool,

        /// Force reinstall even if already on the latest version
        #[arg(long)]
        force: bool,

        /// Include pre-release versions (e.g. rc candidates)
        #[arg(long)]
        pre: bool,
    },
    /// Run a service on this machine (scribe, distill, or mcp)
    Serve {
        #[command(subcommand)]
        command: super::serve_cmd::ServeCmd,
    },
    /// Manage the server fleet (list, add, remove, enable, disable)
    Server {
        #[command(subcommand)]
        command: super::server_cmd::ServerCmd,
    },
    /// Remote cloud access — enrollment, gateway management
    Cloud {
        #[command(subcommand)]
        command: super::cloud_cmd::CloudCmd,
    },
    /// Install/uninstall MCP server config for Claude Desktop & Code
    Mcp {
        #[command(subcommand)]
        command: super::mcp_cmd::McpCmd,
    },
    /// View and manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Run data migrations
    Migrate {
        #[command(subcommand)]
        command: MigrateAction,
    },
    /// Cross-service pipeline operations (rebuild from papers, etc.)
    Pipeline {
        #[command(subcommand)]
        command: super::pipeline_cmd::PipelineCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum MigrateAction {
    /// Move flat files into 2-char prefix subdirectories (papers/, markdown/, catalog/)
    Sharding,
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
