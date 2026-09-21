use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum PersonalCmd {
    /// Ingest a single file into the personal store.
    ///
    /// Examples:
    ///   hs personal add ~/Downloads/lab-results.pdf
    ///   hs personal add tax-2024.docx --category tax
    Add {
        /// Path to the file to ingest.
        file: std::path::PathBuf,

        /// Override the LLM-picked category. Must be one of the configured categories.
        #[arg(long)]
        category: Option<String>,

        /// Override the LLM-picked title.
        #[arg(long)]
        title: Option<String>,

        /// Replace an existing document with the same stem.
        #[arg(long)]
        force: bool,
    },

    /// List ingested documents.
    List {
        /// Filter by category.
        #[arg(long)]
        category: Option<String>,

        /// Maximum number of results.
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
    },

    /// Semantic search across the personal collection.
    Search {
        /// Query string.
        query: String,

        /// Filter by category.
        #[arg(long)]
        category: Option<String>,

        /// Maximum number of results.
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
    },

    /// Read a document's converted markdown by stem.
    Read {
        /// Document stem (filename without extension).
        stem: String,
    },

    /// Remove a document from Qdrant and disk.
    Delete {
        /// Document stem.
        stem: String,
    },

    /// Re-chunk and re-embed a document. Useful after chunker/embedder changes.
    Reindex {
        /// Document stem.
        stem: String,
    },

    /// View configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the resolved configuration.
    Show,
    /// Print the config file path.
    Path,
}
