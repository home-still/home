# Session 03 Planning: Provider Implementations

**Status:** Foundation complete, ready for implementation phase

---

## Where We Are

### ✅ Completed (Sessions 01-02)

**Phase 1: Core Architecture**
- Workspace structure (core library + api binary)
- Data models: Paper, Author, SearchQuery, SearchResult, DownloadResult, PaperMetadata
- Error handling with smart categorization (Permanent/Transient/RateLimited/CircuitBreaker)
- Trait interfaces: PaperProvider, SearchService, DownloadService

**Phase 2: Resilience Layer**
- Rate limiter (per-provider token bucket)
- Circuit breaker (exponential backoff, failure detection)
- Retry logic (exponential backoff with jitter)
- Configuration system (all magic numbers extracted)

### 📁 Current Project Structure

```
paper-fetch/
├── Cargo.toml                    # Workspace
├── paper-fetch-core/
│   ├── Cargo.toml                # 13 dependencies
│   └── src/
│       ├── lib.rs
│       ├── models.rs             # 6 structs, 1 enum
│       ├── error.rs              # 7 error types + categorization
│       ├── ports/                # Trait definitions
│       │   ├── provider.rs       # PaperProvider trait
│       │   ├── search_service.rs # SearchService trait
│       │   └── download_service.rs
│       └── resilience/           # Production-ready patterns
│           ├── config.rs         # ResilienceConfig ✨ NEW
│           ├── rate_limiter.rs   # Per-provider rate limiting
│           ├── circuit_breaker.rs
│           └── retry.rs
└── api/
    └── Cargo.toml                # Future Poem wrapper
```

**Lines of Code:** ~350 (excluding dependencies)
**Zero warnings, zero errors** ✅

---

## Next: Phase 3 - Provider Implementations

### Goal
Implement concrete providers that fetch papers from academic sources.

### Providers to Build (Priority Order)

#### 1. arXiv Provider (Start Here)
**Why first:**
- No API key required
- Simple Atom XML format
- Well-documented API
- Good for learning the pattern

**API Details:**
- Endpoint: `http://export.arxiv.org/api/query`
- Format: Atom feed (XML)
- Rate limit: ~1 req/3s (conservative)
- Search types: Title, Author, Keywords, DOI
- Free PDF access

**Implementation checklist:**
- [ ] Create `providers/` directory
- [ ] Create `providers/arxiv.rs`
- [ ] Implement `PaperProvider` trait
- [ ] Parse Atom XML to `Paper` structs
- [ ] Handle rate limiting
- [ ] Extract PDF URLs
- [ ] Write unit tests
- [ ] Write integration test (optional, hits real API)

**Dependencies needed:**
```toml
roxmltree = "0.20"  # Already have this
url = "2.0"         # Already have this
```

#### 2. CrossRef Provider
**Why second:**
- REST JSON API (easier than XML)
- No key required (but "polite pool" with email recommended)
- Excellent metadata coverage
- 50 req/s rate limit with polite pool

**API Details:**
- Endpoint: `https://api.crossref.org/works`
- Format: JSON
- Rate limit: 50 req/s with polite pool
- Search types: DOI, Title, Author
- Metadata-focused (no PDFs directly)

#### 3. Semantic Scholar Provider
**Why third:**
- Modern REST API
- Good for CS/ML papers
- API key recommended for higher limits
- Includes citation data

**API Details:**
- Endpoint: `https://api.semanticscholar.org/graph/v1`
- Format: JSON
- Rate limit: 100 req/s with API key
- Rich metadata including citations

---

## Implementation Pattern

Each provider follows this structure:

```rust
// providers/arxiv.rs

use async_trait::async_trait;
use crate::ports::provider::PaperProvider;
use crate::models::*;
use crate::error::*;
use reqwest::Client;

pub struct ArxivProvider {
    client: Client,
    base_url: String,
}

impl ArxivProvider {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap(),
            base_url: "http://export.arxiv.org/api/query".to_string(),
        }
    }

    fn build_query_url(&self, query: &SearchQuery) -> String {
        // Format query based on search type
        // e.g., ti:"query" for title, au:"query" for author
    }

    fn parse_atom_feed(&self, xml: &str) -> Result<Vec<Paper>, PaperFetchError> {
        // Use roxmltree to parse XML
        // Extract: id, title, authors, abstract, published, pdf link
    }
}

#[async_trait]
impl PaperProvider for ArxivProvider {
    fn name(&self) -> &'static str {
        "arxiv"
    }

    fn priority(&self) -> u8 {
        80  // High priority for CS/Physics
    }

    fn supported_search_types(&self) -> Vec<SearchType> {
        vec![
            SearchType::Keywords,
            SearchType::Title,
            SearchType::Author,
            SearchType::DOI,
        ]
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperFetchError> {
        let url = self.build_query_url(query);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(PaperFetchError::ProviderUnavailable(
                format!("arXiv returned {}", response.status())
            ));
        }

        let xml = response.text().await?;
        let papers = self.parse_atom_feed(&xml)?;

        Ok(SearchResult {
            papers,
            total_results: 0,  // arXiv doesn't provide total
            next_offset: Some(query.offset + query.max_results),
            provider: "arxiv".to_string(),
        })
    }

    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperFetchError> {
        // Search by DOI
        let query = SearchQuery {
            query: doi.to_string(),
            search_type: SearchType::DOI,
            max_results: 1,
            offset: 0,
        };

        let result = self.search(&query).await?;
        Ok(result.papers.into_iter().next())
    }
}
```

---

## arXiv API Reference

### Search Query Format

**Base URL:** `http://export.arxiv.org/api/query?search_query={query}&start={offset}&max_results={limit}`

**Query Prefixes:**
- `ti:` - Title search
- `au:` - Author search
- `abs:` - Abstract search
- `all:` - All fields (default)

**Example:**
```
http://export.arxiv.org/api/query?search_query=ti:%22neural%20networks%22&start=0&max_results=10
```

### Response Format (Atom XML)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1234.5678v1</id>
    <title>Paper Title Here</title>
    <author>
      <name>Author Name</name>
    </author>
    <summary>Abstract text here...</summary>
    <published>2023-01-15T12:00:00Z</published>
    <link href="http://arxiv.org/abs/1234.5678v1" rel="alternate" type="text/html"/>
    <link href="http://arxiv.org/pdf/1234.5678v1" title="pdf" rel="related" type="application/pdf"/>
  </entry>
</feed>
```

### Important Notes

- **Rate Limiting:** arXiv asks for ~3 seconds between requests
- **User-Agent:** Set a descriptive user agent
- **PDF URLs:** Convert `/abs/` to `/pdf/` or use `<link title="pdf">`
- **Versioning:** IDs include version (e.g., `1234.5678v1`)
- **Max Results:** Limit to reasonable values (10-100)

---

## Testing Strategy

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_query_url() {
        let provider = ArxivProvider::new();
        let query = SearchQuery {
            query: "neural networks".to_string(),
            search_type: SearchType::Title,
            max_results: 10,
            offset: 0,
        };

        let url = provider.build_query_url(&query);
        assert!(url.contains("ti:%22neural%20networks%22"));
    }

    #[test]
    fn test_parse_atom_feed() {
        let provider = ArxivProvider::new();
        let xml = r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
                <entry>
                    <id>http://arxiv.org/abs/1234.5678v1</id>
                    <title>Test Paper</title>
                    <!-- ... -->
                </entry>
            </feed>
        "#;

        let papers = provider.parse_atom_feed(xml).unwrap();
        assert_eq!(papers.len(), 1);
        assert_eq!(papers[0].title, "Test Paper");
    }
}
```

### Integration Test (Optional)
```rust
#[tokio::test]
async fn test_arxiv_search_real_api() {
    let provider = ArxivProvider::new();
    let query = SearchQuery {
        query: "attention is all you need".to_string(),
        search_type: SearchType::Title,
        max_results: 5,
        offset: 0,
    };

    let result = provider.search(&query).await.unwrap();
    assert!(!result.papers.is_empty());
}
```

---

## Dependencies to Add

For arXiv specifically:
```toml
# Already have:
roxmltree = "0.20"
reqwest = { version = "0.12.23", features = ["json", "stream"] }
url = "2.0"

# May need:
urlencoding = "2.1"  # For query parameter encoding
```

---

## Session 03 Goals

**Minimum:**
- [ ] Implement ArxivProvider
- [ ] Parse Atom XML successfully
- [ ] Handle search by title, author, keywords
- [ ] Extract PDF URLs
- [ ] Write basic tests

**Stretch:**
- [ ] Add CrossRef provider
- [ ] Create provider registry/factory
- [ ] Implement provider health checks
- [ ] Add integration tests

---

## Common Pitfalls to Avoid

1. **XML Namespaces:** Remember to handle Atom namespace in roxmltree
2. **URL Encoding:** Properly encode query parameters
3. **Rate Limiting:** Don't forget to integrate ProviderRateLimiter
4. **Error Handling:** Map HTTP errors to our PaperFetchError types
5. **Date Parsing:** arXiv uses ISO 8601 format - use chrono
6. **PDF URLs:** Normalize relative paths to absolute URLs

---

## Resources

**arXiv API:**
- Documentation: https://arxiv.org/help/api
- Rate limits: https://arxiv.org/help/api/user-manual#31-rate-limiting
- Query construction: https://arxiv.org/help/api/user-manual#query_details

**Rust Crates:**
- roxmltree: https://docs.rs/roxmltree/latest/roxmltree/
- reqwest: https://docs.rs/reqwest/latest/reqwest/
- chrono: https://docs.rs/chrono/latest/chrono/

---

## Quick Start Commands for Next Session

```bash
# Navigate to project
cd ~/paper-fetch

# Check current status
cargo check

# Create providers module
mkdir paper-fetch-core/src/providers
touch paper-fetch-core/src/providers/mod.rs
touch paper-fetch-core/src/providers/arxiv.rs

# Add to lib.rs
echo "pub mod providers;" >> paper-fetch-core/src/lib.rs

# Start implementing!
```

---

---

## Future: CLI with Clap (Phase 6)

Once we have service implementations, we'll create a CLI binary using `clap`.

### Dependency
```toml
clap = { version = "4.0", features = ["derive"] }
```

### CLI Structure
```rust
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "paper-fetch")]
#[command(about = "Academic paper search and retrieval tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Download directory
    #[arg(short = 'd', long, default_value = "./downloads")]
    download_path: PathBuf,

    /// Cache directory
    #[arg(short = 'c', long, default_value = "./cache")]
    cache_path: PathBuf,
}

#[derive(Parser)]
enum Commands {
    /// Search for papers
    Search {
        /// Search query
        query: String,

        /// Search type: keywords, title, author, doi
        #[arg(short, long, default_value = "keywords")]
        search_type: String,

        /// Maximum results
        #[arg(short, long, default_value_t = 10)]
        max_results: usize,
    },

    /// Download a paper by DOI
    Download {
        /// Paper DOI
        doi: String,
    },

    /// Extract metadata from local PDF
    Metadata {
        /// Path to PDF file
        file: PathBuf,
    },
}
```

### Example Usage
```bash
# Search for papers
paper-fetch search "neural networks" --search-type title --max-results 20

# Download by DOI
paper-fetch download 10.1234/example.doi

# Custom download path
paper-fetch --download-path ~/Papers download 10.1234/example.doi

# Extract metadata
paper-fetch metadata ~/Papers/paper.pdf
```

### Integration with Config
```rust
fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = Config::default()
        .with_download_path(cli.download_path)
        .with_cache_path(cli.cache_path);

    match cli.command {
        Commands::Search { query, search_type, max_results } => {
            // Initialize services with config
            // Execute search
        }
        Commands::Download { doi } => {
            // Initialize download service with config
            // Download to config.download_path
        }
        Commands::Metadata { file } => {
            // Extract metadata
        }
    }
}
```

**Note:** CLI implementation comes after:
- Provider implementations (Phase 3)
- Service implementations (Phase 4)
- API layer (Phase 5)

---

**Ready to build!** 🚀
