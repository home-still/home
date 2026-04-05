use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DistillCmd {
    /// Set up Qdrant and distill server environment
    Init {
        /// Re-create compose config even if it exists
        #[arg(long)]
        force: bool,
    },
    /// Manage the distill server (Qdrant + native binary)
    Server {
        #[command(subcommand)]
        action: DistillServerAction,
    },
    /// Index markdown files into Qdrant via distill server
    Index {
        /// Re-index all documents (ignore cache)
        #[arg(long)]
        force: bool,

        /// Only index specific files
        #[arg(long)]
        file: Option<Vec<PathBuf>>,

        /// Override server URL
        #[arg(long)]
        server: Option<String>,

        /// Don't yield GPU to scribe when it has work queued
        #[arg(long)]
        no_yield: bool,

        /// Internal: run as daemon child process
        #[arg(long, hide = true)]
        daemon_child: bool,
    },
    /// Semantic search across indexed documents
    Search {
        /// Search query text
        query: String,

        /// Number of results
        #[arg(short, long, default_value = "10")]
        limit: u64,

        /// Filter by year (e.g., ">2020")
        #[arg(long)]
        year: Option<String>,

        /// Filter by topic
        #[arg(long)]
        topic: Option<String>,

        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Show distill system status (Qdrant, server, collection)
    Status {
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum DistillServerAction {
    /// Start Qdrant container and distill server
    Start,
    /// Stop Qdrant container and distill server
    Stop,
    /// Health-check the distill server
    Ping {
        /// Server URL (default: localhost:7434)
        url: Option<String>,
    },
}
