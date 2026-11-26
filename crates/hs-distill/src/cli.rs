use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DistillCmd {
    /// Index markdown files into the myelin store via distill server
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

        /// Store to search, by name (`academic`, `abstracts`, `references`,
        /// `personal`, `web`, `syntheses`). Repeat it to search several.
        /// Without it the server searches its default stores.
        #[arg(long = "store", value_name = "NAME")]
        stores: Vec<String>,

        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Show distill system status (server, backend, the `academic` store)
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
    /// Reconcile markdown ↔ myelin store ↔ catalog. Finds docs that were
    /// embedded but lost their catalog stamp (`--fix-stamps` backfills)
    /// and docs whose markdown exists but never reached the store
    /// (`--reembed` re-indexes). Safe to run anytime: default is dry-run.
    Reconcile {
        /// Backfill missing `embedding` stamps for docs already in the store
        #[arg(long)]
        fix_stamps: bool,
        /// Re-index markdown that never reached the store
        #[arg(long)]
        reembed: bool,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Index documents of the `academic` store again from their stored
    /// markdown and catalog entry, replacing their records in place. A
    /// document's new records are written before its earlier ones are
    /// deleted, so it stays searchable throughout.
    Reindex {
        /// Stems of the documents to index again
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        stems: Vec<String>,
        /// Index every document that has markdown
        #[arg(long)]
        all: bool,
        /// With `--all`, leave out the documents indexed at or after this
        /// time (RFC 3339); name the time a stopped run began to continue it
        #[arg(long, value_name = "TIME", requires = "all")]
        older_than: Option<String>,
        /// How many documents are indexed at once
        #[arg(long, default_value = "1")]
        concurrency: std::num::NonZeroUsize,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Index the converted documents that were never embedded: those whose
    /// catalog entry has a conversion stamp and no embedding stamp with
    /// chunks. Without `--dry-run` it indexes them.
    Backfill {
        /// Report what would be indexed without sending anything to the
        /// server or writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Index at most this many documents. Default: all.
        #[arg(long)]
        limit: Option<usize>,
        /// Also index documents whose last indexing found nothing to index
        #[arg(long)]
        retry_skipped: bool,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Report stored markdown that the repetition cleaner would cut heavily,
    /// which is how a looping conversion shows. Reads only; it changes
    /// nothing and prints a JSON report.
    ScanRepetitions {
        /// Scan at most this many documents. Default: 100000.
        #[arg(long)]
        limit: Option<usize>,
        /// Report documents cut more than this many times. Default: 20.
        #[arg(long)]
        threshold: Option<usize>,
    },
    /// Delete every stored chunk for a doc_id. CLI-only (no MCP) because
    /// this is a destructive write — use to clear orphaned chunks after
    /// markdown has been removed, or to force a clean re-index.
    Purge {
        /// doc_id whose chunks should be deleted
        doc_id: String,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Manage the `abstracts` store — a per-paper
    /// (not per-chunk) embedding of `{title}\n\n{abstract}` for high-
    /// precision semantic search over papers' core arguments. Distinct
    /// from the `academic` store (per-chunk body text). Abstract source is
    /// `openalex` (DOI lookup) > `catalog` (provider abstract) > `markdown`
    /// (`## Abstract` extraction); a paper with none of them is skipped.
    #[command(subcommand)]
    Abstracts(AbstractsCmd),
}

#[derive(Subcommand, Debug)]
pub enum AbstractsCmd {
    /// Iterate every catalog entry, coalesce title+abstract, embed into
    /// the `abstracts` store, and stamp the catalog. Idempotent
    /// — entries with an `abstract_embed` stamp are skipped unless
    /// `--force`.
    Build {
        /// Re-embed entries that already have an `abstract_embed` stamp
        #[arg(long)]
        force: bool,
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Embed only catalog entries that lack an `abstract_embed` stamp
    /// (alias for `build` without `--force`). Use this for incremental
    /// catch-up after new papers are downloaded.
    Reconcile {
        /// Override server URL
        #[arg(long)]
        server: Option<String>,
    },
    /// Report coverage: total catalog entries vs. abstract_embed stamps
    /// broken down by source (openalex / catalog / markdown).
    Status,
}
