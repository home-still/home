use clap::{Subcommand, ValueEnum};

#[derive(Subcommand, Debug)]
pub enum PaperCmd {
    /// Search for papers across providers
    #[command(after_help = "\
  Examples:
    hs paper search \"transformer attention\"
    hs paper search --type author \"Hinton\" -n 5")]
    Search {
        /// Search query string
        query: String,

        /// Type of search to perform
        #[arg(short = 't', long = "type", default_value = "keywords")]
        search_type: SearchTypeArg,

        /// Show abstracts in search results
        #[arg(short = 'a', long = "abstract")]
        show_abstract: bool,

        /// Filter by date range (e.g., ">=2025", ">2023 <2025", ">=2024-06")
        #[arg(short = 'd', long = "date")]
        date: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value = "10", value_parser = clap::value_parser!(u16).range(1..))]
        max_results: u16,

        /// Pagination offset. OpenAlex needs a multiple of the page size;
        /// Europe PMC cannot page by offset (rejected alone, skipped in `all`)
        #[arg(long, default_value = "0")]
        offset: usize,

        /// Provider to search
        #[arg(short, long, default_value = "all")]
        provider: ProviderArg,

        /// Sort result by: relevance (default), date, citations
        #[arg(short = 's', long = "sort", default_value = "relevance")]
        sort_by: SortByArg,

        /// Minimum citation count to include a result
        #[arg(long)]
        min_citations: Option<u64>,
    },
    /// Get a single paper by DOI
    ///
    /// Examples:
    ///   paper get --doi "10.48550/arXiv.2301.00001"
    Get {
        /// DOI to look up
        #[arg(long)]
        doi: String,

        /// Provider to query. `all` (default) asks every provider; an arXiv
        /// DOI goes to arXiv alone, since no other provider indexes it.
        #[arg(short, long, default_value = "all")]
        provider: ProviderArg,
    },
    /// Download papers (search + download, or single DOI)
    ///
    /// Examples:
    ///   paper download "neural nets" -n 25
    ///   paper download --doi "10.48550/arXiv.2301.00001"
    Download {
        /// Search query (downloads matching papers)
        query: Option<String>,

        /// Filter by date range (e.g., ">=2025", ">2023 <2025", ">=2024-06")
        #[arg(short = 'd', long = "date")]
        date: Option<String>,

        /// Download a single paper by DOI
        #[arg(long, conflicts_with = "query")]
        doi: Option<String>,

        /// Maximum number of papers to download
        #[arg(short = 'n', long, default_value = "10", value_parser = clap::value_parser!(u16).range(1..))]
        max_results: u16,

        /// Maximum concurrent downloads
        #[arg(short = 'c', long, default_value = "4")]
        concurrency: usize,

        /// Search type for query-based download
        #[arg(short = 't', long = "type", default_value = "keywords")]
        search_type: SearchTypeArg,

        /// Provider to search
        #[arg(short, long, default_value = "all")]
        provider: ProviderArg,
    },
    /// View and manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the resolved configuration
    Show,
    /// Print the config file path
    Path,
}

#[derive(ValueEnum, Clone, Debug)]
#[value(rename_all = "lowercase")]
pub enum SearchTypeArg {
    Keywords,
    Title,
    Author,
    Doi,
    Subject,
}

#[derive(ValueEnum, Clone, Debug, Default)]
#[value(rename_all = "lowercase")]
pub enum SortByArg {
    #[default]
    Relevance,
    Date,
    Citations,
}

/// `--provider` values. Besides clap's lowercase variant names, each provider
/// answers to the name the README, the MCP tools and the error messages use
/// (`semantic_scholar`, `europe_pmc` / `europmc`), so a name copied from any
/// of them works here.
#[derive(ValueEnum, Clone, Debug)]
#[value(rename_all = "lowercase")]
pub enum ProviderArg {
    All,
    Arxiv,
    OpenAlex,
    #[value(alias = "semantic_scholar", alias = "s2")]
    SemanticScholar,
    #[value(alias = "europe_pmc", alias = "europmc", alias = "pmc")]
    EuropePmc,
    CrossRef,
    Core,
}

impl From<SearchTypeArg> for crate::models::SearchType {
    fn from(arg: SearchTypeArg) -> Self {
        match arg {
            SearchTypeArg::Keywords => Self::Keywords,
            SearchTypeArg::Title => Self::Title,
            SearchTypeArg::Author => Self::Author,
            SearchTypeArg::Doi => Self::DOI,
            SearchTypeArg::Subject => Self::Subject,
        }
    }
}

impl From<SortByArg> for crate::models::SortBy {
    fn from(arg: SortByArg) -> Self {
        match arg {
            SortByArg::Relevance => Self::Relevance,
            SortByArg::Date => Self::Date,
            SortByArg::Citations => Self::Citations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The README documents `semantic_scholar` and `europe_pmc`, the MCP tools
    /// and rate-limit errors say `europmc`: clap's own spelling is
    /// `semanticscholar` / `europepmc`, which used to be the only ones accepted.
    #[test]
    fn every_documented_provider_spelling_is_accepted() {
        for name in [
            "all",
            "arxiv",
            "openalex",
            "semanticscholar",
            "semantic_scholar",
            "s2",
            "europepmc",
            "europe_pmc",
            "europmc",
            "pmc",
            "crossref",
            "core",
        ] {
            assert!(
                <ProviderArg as ValueEnum>::from_str(name, false).is_ok(),
                "{name}"
            );
        }
        assert!(<ProviderArg as ValueEnum>::from_str("scholar", false).is_err());
    }
}
