use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DistillCmd {
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
    /// Show index status (collection info, document count)
    Status {
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
}
