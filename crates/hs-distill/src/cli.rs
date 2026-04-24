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
    /// Subscribe to `scribe.completed` on the configured event bus and index
    /// each markdown object via the distill server. Event-driven replacement
    /// for the directory-walk indexer.
    WatchEvents {
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Explain why a stem would (or would not) produce chunks: fetch its
    /// markdown, run the chunker + quality filter, and print per-chunk
    /// accept/reject reasons.
    Diagnose {
        /// Paper stem (filename without extension)
        stem: String,
        /// Show the full chunk text for each chunk
        #[arg(long)]
        verbose: bool,
    },
    /// Reconcile markdown ↔ Qdrant ↔ catalog. Finds docs that were
    /// embedded but lost their catalog stamp (`--fix-stamps` backfills)
    /// and docs whose markdown exists but never reached Qdrant
    /// (`--reembed` re-indexes). Safe to run anytime: default is dry-run.
    Reconcile {
        /// Backfill missing `embedding` stamps for docs already in Qdrant
        #[arg(long)]
        fix_stamps: bool,
        /// Re-index markdown that never reached Qdrant
        #[arg(long)]
        reembed: bool,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
}
