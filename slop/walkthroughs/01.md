# Research Hub Service - Clean Reimplementation Guide

## Overview

**What We're Building:**
A clean, agnostic academic paper search and retrieval service that can be wrapped with any transport layer (HTTP API via Poem, MCP server, CLI, etc.).

**Why This Architecture:**
- **Separation of Concerns**: Core business logic isolated from transport/protocol concerns
- **Testability**: Service layer can be tested without spinning up servers
- **Flexibility**: Same service powers multiple interfaces (API, MCP, CLI)
- **Maintainability**: Clear boundaries make changes predictable

**Original Project Analysis:**
The [research_hub_mcp](https://github.com/Ladvien/research_hub_mcp) is a sophisticated MCP server for academic research with:
- 14 provider integrations (arXiv, CrossRef, Semantic Scholar, PubMed, etc.)
- Intelligent meta-search with provider prioritization
- Resilience patterns (circuit breakers, rate limiting, retries)
- PDF download with multi-provider fallback
- Metadata extraction from PDFs
- Bibliography generation in multiple formats

---

## Architecture Philosophy

### Hexagonal Architecture (Ports & Adapters)

```
┌─────────────────────────────────────────┐
│         Transport Layer (Poem API)      │  ← User-facing
│  POST /search, GET /download, etc.      │
└────────────────┬────────────────────────┘
                 │
┌────────────────▼────────────────────────┐
│         Service Layer (Core)            │  ← Business logic
│  SearchService, DownloadService, etc.   │
└────────────────┬────────────────────────┘
                 │
┌────────────────▼────────────────────────┐
│      Provider Adapters (External)       │  ← Infrastructure
│  ArxivClient, CrossRefClient, etc.      │
└─────────────────────────────────────────┘
```

**Key Principles:**
1. **Service Layer** contains ALL business logic and is transport-agnostic
2. **Trait-based interfaces** define contracts (ports)
3. **Concrete implementations** (adapters) satisfy those contracts
4. **Dependency Injection** wires everything together

---

## Dependencies

### Core Service Dependencies
```toml
[dependencies]
# Async Runtime
tokio = { version = "1.0", features = ["full"] }
async-trait = "0.1"

# Serialization (transport-agnostic data structures)
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# Error Handling
thiserror = "1.0"
anyhow = "1.0"

# HTTP Client (for providers)
reqwest = { version = "0.11", features = ["json", "stream", "gzip"] }

# HTML/XML Parsing
scraper = "0.19"
roxmltree = "0.20"

# PDF Processing
lopdf = "0.34"
regex = "1.0"

# Resilience
tokio-retry = "0.3"
backoff = "0.4"
failsafe = "1.3"

# Rate Limiting
governor = "0.6"  # Better than custom implementation

# Caching
sled = "0.34"

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# Utilities
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1.0", features = ["v4", "serde"] }
url = "2.0"
sha2 = "0.10"
```

### API Layer Dependencies (Separate)
```toml
[dependencies]
poem = { version = "3.0", features = ["json", "multipart"] }
poem-openapi = "5.0"

# Reference core service
paper-fetch-core = { path = "../core" }
```

**Why Separate?**
The core service doesn't depend on Poem. The API wrapper depends on both.

---

## Project Structure

```
paper-fetch/
├── Cargo.toml                    # Workspace definition
├── core/                         # Core service (agnostic)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs
│   │   ├── models.rs            # Shared data types
│   │   ├── error.rs             # Error types & categories
│   │   ├── ports/               # Trait definitions
│   │   │   ├── mod.rs
│   │   │   ├── search_service.rs
│   │   │   ├── download_service.rs
│   │   │   ├── metadata_service.rs
│   │   │   └── provider.rs
│   │   ├── services/            # Business logic
│   │   │   ├── mod.rs
│   │   │   ├── search.rs
│   │   │   ├── download.rs
│   │   │   ├── metadata.rs
│   │   │   └── meta_search.rs
│   │   ├── providers/           # Provider adapters
│   │   │   ├── mod.rs
│   │   │   ├── traits.rs
│   │   │   ├── arxiv.rs
│   │   │   ├── crossref.rs
│   │   │   ├── semantic_scholar.rs
│   │   │   └── ...
│   │   ├── resilience/          # Reliability patterns
│   │   │   ├── mod.rs
│   │   │   ├── circuit_breaker.rs
│   │   │   ├── rate_limiter.rs
│   │   │   └── retry.rs
│   │   └── utils/               # Helpers
│   │       ├── mod.rs
│   │       ├── cache.rs
│   │       └── validation.rs
│   └── tests/                   # Integration tests
│       └── ...
├── api/                          # Poem HTTP API wrapper
│   ├── Cargo.toml
│   ├── src/
│   │   ├── main.rs
│   │   ├── routes/
│   │   │   ├── mod.rs
│   │   │   ├── search.rs
│   │   │   ├── download.rs
│   │   │   └── metadata.rs
│   │   └── middleware/
│   │       └── ...
│   └── tests/
│       └── ...
└── mcp/                          # MCP server wrapper (optional)
    ├── Cargo.toml
    └── src/
        └── main.rs
```

**Key Points:**
- `core/` is a library crate with no transport dependencies
- `api/` and `mcp/` are binary crates that wrap `core`
- Each layer can be tested independently

---

## Implementation Steps

### Phase 1: Core Data Models (Week 1)

#### Step 1: Define Core Types

**File:** `core/src/models.rs`

**What:**
Transport-agnostic data structures representing papers, search queries, results, and metadata.

**Types to Define:**
```rust
// Paper representation
pub struct Paper {
    pub id: String,
    pub title: String,
    pub authors: Vec<Author>,
    pub abstract_text: Option<String>,
    pub doi: Option<String>,
    pub publication_date: Option<chrono::NaiveDate>,
    pub pdf_url: Option<String>,
    pub source: String,  // "arxiv", "crossref", etc.
    // ... more fields
}

pub struct Author {
    pub name: String,
    pub affiliation: Option<String>,
    pub orcid: Option<String>,
}

// Search types
pub enum SearchType {
    Keywords,
    Title,
    Author,
    DOI,
    Subject,
}

pub struct SearchQuery {
    pub query: String,
    pub search_type: SearchType,
    pub max_results: usize,
    pub offset: usize,
}

pub struct SearchResult {
    pub papers: Vec<Paper>,
    pub total_results: usize,
    pub next_offset: Option<usize>,
    pub provider: String,
}

// Metadata
pub struct PaperMetadata {
    pub title: Option<String>,
    pub authors: Vec<Author>,
    pub doi: Option<String>,
    pub references: Vec<Reference>,
    pub confidence_score: f32,
    // ... more fields
}

// Download info
pub struct DownloadResult {
    pub file_path: PathBuf,
    pub doi: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
}
```

**Implementation Tips:**
- Use `#[derive(Debug, Clone, Serialize, Deserialize)]` on all types
- Keep it simple initially, add fields as needed
- Make fields `pub` for now (we can encapsulate later)

---

#### Step 2: Error Types with Categories

**File:** `core/src/error.rs`

**What:**
Comprehensive error handling that supports resilience strategies.

**Pattern:**
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PaperFetchError {
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Provider unavailable: {0}")]
    ProviderUnavailable(String),

    #[error("Rate limited: retry after {retry_after:?}")]
    RateLimited {
        provider: String,
        retry_after: Option<std::time::Duration>
    },

    #[error("Circuit breaker open: {0}")]
    CircuitBreakerOpen(String),

    // ... more variants
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorCategory {
    Permanent,        // Don't retry
    Transient,        // Retry with backoff
    RateLimited,      // Retry after delay
    CircuitBreaker,   // Stop temporarily
}

impl PaperFetchError {
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidInput(_) => ErrorCategory::Permanent,
            Self::Http(e) if e.is_timeout() => ErrorCategory::Transient,
            Self::RateLimited { .. } => ErrorCategory::RateLimited,
            Self::CircuitBreakerOpen(_) => ErrorCategory::CircuitBreaker,
            // ... more patterns
        }
    }

    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}
```

**Why:**
Error categorization enables smart retry logic without coupling error types to resilience mechanisms.

---

### Phase 2: Define Ports (Trait Interfaces) (Week 1)

#### Step 3: Provider Trait

**File:** `core/src/ports/provider.rs`

**What:**
The contract every provider (arXiv, CrossRef, etc.) must fulfill.

**Pattern:**
```rust
use async_trait::async_trait;
use crate::models::*;
use crate::error::*;

#[async_trait]
pub trait PaperProvider: Send + Sync {
    /// Unique identifier (e.g., "arxiv", "crossref")
    fn name(&self) -> &'static str;

    /// Priority for meta-search (0-255, higher = earlier)
    fn priority(&self) -> u8 {
        100
    }

    /// Supported search types
    fn supported_search_types(&self) -> Vec<SearchType>;

    /// Main search implementation
    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperFetchError>;

    /// Optional: Get paper by DOI
    async fn get_by_doi(&self, doi: &str) -> Result<Option<Paper>, PaperFetchError> {
        Ok(None)  // Default: not supported
    }

    /// Optional: Get PDF URL if available
    async fn get_pdf_url(&self, paper: &Paper) -> Result<Option<String>, PaperFetchError> {
        Ok(None)  // Default: not supported
    }

    /// Health check
    async fn health_check(&self) -> Result<(), PaperFetchError> {
        Ok(())  // Default: assume healthy
    }
}
```

**Why:**
- `async_trait` enables async methods in traits
- Default implementations reduce boilerplate
- `Send + Sync` ensures thread safety

---

#### Step 4: Service Traits

**File:** `core/src/ports/search_service.rs`

**What:**
High-level search service interface.

**Pattern:**
```rust
use async_trait::async_trait;
use crate::models::*;
use crate::error::*;

#[async_trait]
pub trait SearchService: Send + Sync {
    /// Search across all providers (meta-search)
    async fn search_all(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, PaperFetchError>;

    /// Search specific provider
    async fn search_provider(
        &self,
        provider: &str,
        query: &SearchQuery
    ) -> Result<SearchResult, PaperFetchError>;

    /// Health check for all providers
    async fn health_check(&self) -> HealthStatus;
}

pub struct HealthStatus {
    pub healthy_providers: Vec<String>,
    pub unhealthy_providers: Vec<(String, String)>,  // (name, error)
}
```

**Files:** `core/src/ports/download_service.rs`, `core/src/ports/metadata_service.rs`

Similar patterns for download and metadata extraction.

---

### Phase 3: Resilience Layer (Week 2)

#### Step 5: Rate Limiter

**File:** `core/src/resilience/rate_limiter.rs`

**What:**
Token bucket rate limiting per provider.

**Pattern:**
```rust
use governor::{Quota, RateLimiter, DefaultDirectRateLimiter};
use std::num::NonZeroU32;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ProviderRateLimiter {
    limiters: Arc<RwLock<HashMap<String, DefaultDirectRateLimiter>>>,
    default_quota: Quota,
}

impl ProviderRateLimiter {
    pub fn new(requests_per_second: u32) -> Self {
        let quota = Quota::per_second(NonZeroU32::new(requests_per_second).unwrap());
        Self {
            limiters: Arc::new(RwLock::new(HashMap::new())),
            default_quota: quota,
        }
    }

    pub async fn acquire(&self, provider: &str) {
        let mut limiters = self.limiters.write().await;
        let limiter = limiters
            .entry(provider.to_string())
            .or_insert_with(|| RateLimiter::direct(self.default_quota));

        limiter.until_ready().await;
    }
}
```

**Why:**
The `governor` crate provides production-ready rate limiting with minimal code.

---

#### Step 6: Circuit Breaker

**File:** `core/src/resilience/circuit_breaker.rs`

**What:**
Prevents cascading failures by stopping requests to failing providers.

**Pattern:**
```rust
use failsafe::{CircuitBreaker, Config};
use std::time::Duration;

pub struct ProviderCircuitBreaker {
    breaker: CircuitBreaker,
}

impl ProviderCircuitBreaker {
    pub fn new() -> Self {
        let config = Config::new()
            .failure_rate_threshold(50)  // 50% failure opens circuit
            .sample_size(10)             // Last 10 requests
            .timeout(Duration::from_secs(30));  // Stay open 30s

        Self {
            breaker: config.build(),
        }
    }

    pub async fn call<F, T, E>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: std::error::Error,
    {
        self.breaker
            .call(f)
            .map_err(|e| /* convert failsafe error */)
    }
}
```

**Note:** The `failsafe` crate handles state transitions (closed → open → half-open).

---

#### Step 7: Retry Logic

**File:** `core/src/resilience/retry.rs`

**What:**
Exponential backoff with jitter for transient failures.

**Pattern:**
```rust
use backoff::{ExponentialBackoff, future::retry};
use crate::error::{PaperFetchError, ErrorCategory};

pub async fn retry_with_backoff<F, Fut, T>(
    operation: F,
) -> Result<T, PaperFetchError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, PaperFetchError>>,
{
    let backoff = ExponentialBackoff {
        max_elapsed_time: Some(std::time::Duration::from_secs(60)),
        ..Default::default()
    };

    retry(backoff, operation, |err: &PaperFetchError| {
        match err.category() {
            ErrorCategory::Transient => Ok(()),  // Retry
            _ => Err(backoff::Error::Permanent(err.clone())),  // Stop
        }
    })
    .await
}
```

**Why:**
The `backoff` crate handles timing; our `ErrorCategory` decides *what* to retry.

---

### Phase 4: Provider Implementations (Week 3-4)

#### Step 8: arXiv Provider

**File:** `core/src/providers/arxiv.rs`

**What:**
Concrete implementation of `PaperProvider` for arXiv.

**Structure:**
```rust
use async_trait::async_trait;
use crate::ports::provider::PaperProvider;
use crate::models::*;
use crate::error::*;
use reqwest::Client;
use roxmltree::Document;

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
        let search_term = match query.search_type {
            SearchType::Title => format!("ti:{}", query.query),
            SearchType::Author => format!("au:{}", query.query),
            SearchType::Keywords => format!("all:{}", query.query),
            _ => query.query.clone(),
        };

        format!(
            "{}?search_query={}&start={}&max_results={}",
            self.base_url,
            urlencoding::encode(&search_term),
            query.offset,
            query.max_results
        )
    }

    fn parse_atom_feed(&self, xml: &str) -> Result<Vec<Paper>, PaperFetchError> {
        let doc = Document::parse(xml)
            .map_err(|e| PaperFetchError::ParseError(e.to_string()))?;

        // Parse <entry> elements into Paper structs
        // Extract: id, title, authors, abstract, published, pdf link
        // ... implementation details ...

        Ok(papers)
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
        ]
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchResult, PaperFetchError> {
        let url = self.build_query_url(query);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(PaperFetchError::from)?;

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
}
```

**Gotchas:**
- arXiv uses Atom feeds (XML), not JSON
- No authentication required
- Rate limit: ~1 req/sec (be conservative)
- PDF URLs need normalization (`/pdf/` → `https://arxiv.org/pdf/`)

**Repeat Pattern:**
Implement `CrossrefProvider`, `SemanticScholarProvider`, etc. with same structure.

---

#### Step 9: Provider Registry

**File:** `core/src/providers/mod.rs`

**What:**
Central registry for all providers.

**Pattern:**
```rust
use crate::ports::provider::PaperProvider;
use std::sync::Arc;

mod arxiv;
mod crossref;
// ... more providers

pub use arxiv::ArxivProvider;
pub use crossref::CrossrefProvider;

pub fn default_providers() -> Vec<Arc<dyn PaperProvider>> {
    vec![
        Arc::new(ArxivProvider::new()),
        Arc::new(CrossrefProvider::new()),
        // ... add all providers
    ]
}
```

**Why:**
Centralized initialization keeps service setup simple.

---

### Phase 5: Service Implementations (Week 5)

#### Step 10: Meta-Search Service

**File:** `core/src/services/search.rs`

**What:**
Orchestrates parallel searches across providers with intelligent ranking.

**Core Logic:**
```rust
use crate::ports::{SearchService, PaperProvider};
use crate::models::*;
use crate::error::*;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::task::JoinSet;

pub struct MetaSearchService {
    providers: Vec<Arc<dyn PaperProvider>>,
    rate_limiter: Arc<ProviderRateLimiter>,
    circuit_breakers: HashMap<String, ProviderCircuitBreaker>,
}

impl MetaSearchService {
    pub fn new(providers: Vec<Arc<dyn PaperProvider>>) -> Self {
        let rate_limiter = Arc::new(ProviderRateLimiter::new(1));  // 1 req/sec default

        let circuit_breakers = providers
            .iter()
            .map(|p| (p.name().to_string(), ProviderCircuitBreaker::new()))
            .collect();

        Self {
            providers,
            rate_limiter,
            circuit_breakers,
        }
    }

    async fn search_provider_with_resilience(
        &self,
        provider: Arc<dyn PaperProvider>,
        query: SearchQuery,
    ) -> Result<SearchResult, PaperFetchError> {
        let provider_name = provider.name();

        // Rate limiting
        self.rate_limiter.acquire(provider_name).await;

        // Circuit breaker
        let breaker = self.circuit_breakers.get(provider_name).unwrap();

        breaker.call(|| async {
            // Retry logic
            retry_with_backoff(|| async {
                provider.search(&query).await
            }).await
        }).await
    }
}

#[async_trait]
impl SearchService for MetaSearchService {
    async fn search_all(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, PaperFetchError> {
        let mut tasks = JoinSet::new();

        // Sort providers by priority
        let mut sorted_providers = self.providers.clone();
        sorted_providers.sort_by_key(|p| std::cmp::Reverse(p.priority()));

        // Launch parallel searches
        for provider in sorted_providers {
            let query = query.clone();
            let service = self.clone();  // Need Arc wrapper

            tasks.spawn(async move {
                service.search_provider_with_resilience(provider, query).await
            });
        }

        // Collect results (continue on individual failures)
        let mut results = Vec::new();
        while let Some(res) = tasks.join_next().await {
            if let Ok(Ok(result)) = res {
                results.push(result);
            }
            // Log failures but don't fail entire operation
        }

        Ok(results)
    }

    async fn search_provider(
        &self,
        provider_name: &str,
        query: &SearchQuery,
    ) -> Result<SearchResult, PaperFetchError> {
        let provider = self.providers
            .iter()
            .find(|p| p.name() == provider_name)
            .ok_or_else(|| PaperFetchError::InvalidInput(
                format!("Unknown provider: {}", provider_name)
            ))?;

        self.search_provider_with_resilience(provider.clone(), query.clone()).await
    }

    async fn health_check(&self) -> HealthStatus {
        // Parallel health checks for all providers
        // ... implementation ...
    }
}
```

**Key Concepts:**
- **Parallel Execution**: `JoinSet` runs providers concurrently
- **Layered Resilience**: Rate limit → Circuit breaker → Retry
- **Graceful Degradation**: Individual failures don't fail the whole search

---

#### Step 11: Download Service

**File:** `core/src/services/download.rs`

**What:**
Handles PDF downloads with multi-provider fallback.

**Pattern:**
```rust
pub struct DownloadService {
    client: Client,
    providers: Vec<Arc<dyn PaperProvider>>,
    download_dir: PathBuf,
}

impl DownloadService {
    pub async fn download_by_doi(&self, doi: &str) -> Result<DownloadResult, PaperFetchError> {
        // 1. Try to get PDF URL from each provider
        let pdf_url = self.get_pdf_url_cascade(doi).await?;

        // 2. Download with streaming
        let file_path = self.download_dir.join(format!("{}.pdf", sanitize_filename(doi)));
        let sha256 = self.stream_download(&pdf_url, &file_path).await?;

        Ok(DownloadResult {
            file_path,
            doi: Some(doi.to_string()),
            sha256,
            size_bytes: /* file size */,
        })
    }

    async fn get_pdf_url_cascade(&self, doi: &str) -> Result<String, PaperFetchError> {
        // Try providers in priority order
        for provider in &self.providers {
            if let Ok(Some(paper)) = provider.get_by_doi(doi).await {
                if let Ok(Some(url)) = provider.get_pdf_url(&paper).await {
                    return Ok(url);
                }
            }
        }

        Err(PaperFetchError::NotFound(format!("No PDF found for DOI: {}", doi)))
    }

    async fn stream_download(&self, url: &str, path: &PathBuf) -> Result<String, PaperFetchError> {
        use tokio::io::AsyncWriteOnce;
        use sha2::{Sha256, Digest};

        let response = self.client.get(url).send().await?;
        let mut file = tokio::fs::File::create(path).await?;
        let mut hasher = Sha256::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.try_next().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }

        Ok(format!("{:x}", hasher.finalize()))
    }
}
```

**Implementation:**
- Streaming prevents memory exhaustion on large PDFs
- SHA256 verification ensures integrity
- Cascade fallback tries multiple providers

---

#### Step 12: Metadata Service

**File:** `core/src/services/metadata.rs`

**What:**
Extracts structured metadata from PDF files.

**Pattern:**
```rust
use lopdf::Document;
use regex::Regex;

pub struct MetadataService {
    title_regex: Regex,
    doi_regex: Regex,
    // ... more regex patterns
}

impl MetadataService {
    pub fn new() -> Self {
        Self {
            title_regex: Regex::new(r"^[A-Z][^\n]{10,200}$").unwrap(),
            doi_regex: Regex::new(r"10\.\d{4,}/[^\s]+").unwrap(),
        }
    }

    pub async fn extract(&self, pdf_path: &Path) -> Result<PaperMetadata, PaperFetchError> {
        // 1. Load PDF
        let doc = Document::load(pdf_path)
            .map_err(|e| PaperFetchError::ParseError(e.to_string()))?;

        // 2. Extract text
        let text = self.extract_text(&doc)?;

        // 3. Apply regex patterns
        let title = self.extract_title(&text);
        let doi = self.extract_doi(&text);
        let authors = self.extract_authors(&text);

        // 4. Calculate confidence score
        let confidence = self.calculate_confidence(&title, &doi, &authors);

        Ok(PaperMetadata {
            title,
            authors,
            doi,
            confidence_score: confidence,
            // ... more fields
        })
    }

    fn extract_text(&self, doc: &Document) -> Result<String, PaperFetchError> {
        // Iterate pages, extract text content
        // lopdf provides page.extract_text()
        // ... implementation ...
    }

    fn calculate_confidence(&self, title: &Option<String>, doi: &Option<String>, authors: &[Author]) -> f32 {
        let mut score = 0.0;
        if title.is_some() { score += 0.3; }
        if doi.is_some() { score += 0.3; }
        if !authors.is_empty() { score += 0.2; }
        // ... more factors
        score
    }
}
```

**Challenges:**
- PDF text extraction is unreliable (formatting issues)
- Regex patterns need tuning per field
- Multi-column layouts require special handling

---

### Phase 6: API Layer with Poem (Week 6)

#### Step 13: API Setup

**File:** `api/src/main.rs`

**What:**
Initialize Poem server with dependency injection.

**Pattern:**
```rust
use poem::{listener::TcpListener, Route, Server, middleware::Tracing};
use poem_openapi::OpenApiService;
use paper_fetch_core::*;
use std::sync::Arc;

mod routes;
use routes::*;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();

    // Initialize core services
    let providers = providers::default_providers();
    let search_service = Arc::new(services::MetaSearchService::new(providers.clone()));
    let download_service = Arc::new(services::DownloadService::new(
        providers,
        PathBuf::from("./downloads"),
    ));
    let metadata_service = Arc::new(services::MetadataService::new());

    // Build API service
    let api_service = OpenApiService::new(
        SearchApi::new(search_service),
        "Paper Fetch API",
        "1.0",
    )
    .server("http://localhost:3000/api");

    let ui = api_service.swagger_ui();
    let spec = api_service.spec_endpoint();

    // Build routes
    let app = Route::new()
        .nest("/api", api_service)
        .nest("/docs", ui)
        .nest("/spec", spec)
        .with(Tracing);

    Server::new(TcpListener::bind("0.0.0.0:3000"))
        .run(app)
        .await
}
```

**Why:**
Services are injected as `Arc<dyn Trait>` for shared state across requests.

---

#### Step 14: Search Endpoint

**File:** `api/src/routes/search.rs`

**What:**
HTTP endpoint wrapping `SearchService`.

**Pattern:**
```rust
use poem_openapi::{payload::Json, OpenApi, Object};
use paper_fetch_core::{SearchService, SearchQuery, SearchType};
use std::sync::Arc;

#[derive(Object)]
struct SearchRequest {
    query: String,
    search_type: String,  // "keywords", "title", etc.
    max_results: Option<usize>,
}

#[derive(Object)]
struct SearchResponse {
    results: Vec<PaperResponse>,
    providers: Vec<String>,
}

#[derive(Object)]
struct PaperResponse {
    title: String,
    authors: Vec<String>,
    doi: Option<String>,
    pdf_url: Option<String>,
    source: String,
}

pub struct SearchApi {
    service: Arc<dyn SearchService>,
}

impl SearchApi {
    pub fn new(service: Arc<dyn SearchService>) -> Self {
        Self { service }
    }
}

#[OpenApi]
impl SearchApi {
    #[oai(path = "/search", method = "post")]
    async fn search(&self, req: Json<SearchRequest>) -> Json<SearchResponse> {
        let query = SearchQuery {
            query: req.query.clone(),
            search_type: parse_search_type(&req.search_type),
            max_results: req.max_results.unwrap_or(20),
            offset: 0,
        };

        let results = self.service.search_all(&query).await.unwrap();

        // Convert core types to API types
        let papers = results
            .iter()
            .flat_map(|r| &r.papers)
            .map(|p| PaperResponse {
                title: p.title.clone(),
                authors: p.authors.iter().map(|a| a.name.clone()).collect(),
                doi: p.doi.clone(),
                pdf_url: p.pdf_url.clone(),
                source: p.source.clone(),
            })
            .collect();

        Json(SearchResponse {
            results: papers,
            providers: results.iter().map(|r| r.provider.clone()).collect(),
        })
    }
}
```

**Key Points:**
- API types (`SearchRequest`) separate from core types (`SearchQuery`)
- Error handling should return proper HTTP status codes
- Add validation, rate limiting, authentication as needed

---

#### Step 15: Download Endpoint

**File:** `api/src/routes/download.rs`

**Pattern:**
```rust
use poem::{Response, Body};
use poem_openapi::{OpenApi, payload::Json};
use tokio_util::io::ReaderStream;

#[OpenApi]
impl DownloadApi {
    #[oai(path = "/download/:doi", method = "get")]
    async fn download_pdf(&self, doi: poem::web::Path<String>) -> Response {
        match self.service.download_by_doi(&doi).await {
            Ok(result) => {
                // Stream file back to client
                let file = tokio::fs::File::open(&result.file_path).await.unwrap();
                let stream = ReaderStream::new(file);

                Response::builder()
                    .header("Content-Type", "application/pdf")
                    .header("Content-Disposition", format!("attachment; filename=\"{}.pdf\"", doi))
                    .body(Body::from_bytes_stream(stream))
            }
            Err(e) => {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(e.to_string())
            }
        }
    }
}
```

**Why:**
Streaming responses prevent memory issues with large PDFs.

---

### Phase 7: Testing & Refinement (Week 7)

#### Step 16: Unit Tests

**Files:** Throughout `core/src/` modules

**Pattern:**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_arxiv_search() {
        let provider = ArxivProvider::new();
        let query = SearchQuery {
            query: "neural networks".to_string(),
            search_type: SearchType::Keywords,
            max_results: 5,
            offset: 0,
        };

        let result = provider.search(&query).await.unwrap();
        assert!(!result.papers.is_empty());
        assert_eq!(result.provider, "arxiv");
    }

    #[test]
    fn test_error_categorization() {
        let err = PaperFetchError::InvalidInput("test".to_string());
        assert!(matches!(err.category(), ErrorCategory::Permanent));

        let err = PaperFetchError::RateLimited {
            provider: "test".to_string(),
            retry_after: None
        };
        assert!(matches!(err.category(), ErrorCategory::RateLimited));
    }
}
```

---

#### Step 17: Integration Tests

**File:** `core/tests/integration_test.rs`

**What:**
End-to-end testing with real providers (use sparingly).

**Pattern:**
```rust
use paper_fetch_core::*;

#[tokio::test]
async fn test_meta_search_integration() {
    let providers = providers::default_providers();
    let service = services::MetaSearchService::new(providers);

    let query = SearchQuery {
        query: "attention is all you need".to_string(),
        search_type: SearchType::Title,
        max_results: 10,
        offset: 0,
    };

    let results = service.search_all(&query).await.unwrap();

    // Should get results from multiple providers
    assert!(results.len() > 1);

    // Should find the famous paper
    let papers: Vec<_> = results.iter().flat_map(|r| &r.papers).collect();
    assert!(papers.iter().any(|p| p.title.contains("Attention")));
}
```

**Use Mocks:**
For CI/CD, mock provider responses to avoid rate limits.

---

## Advanced Topics

### Caching Strategy

**File:** `core/src/utils/cache.rs`

**What:**
Use `sled` for persistent caching of search results.

**Pattern:**
```rust
use sled::Db;
use crate::models::*;

pub struct SearchCache {
    db: Db,
}

impl SearchCache {
    pub fn new(path: &str) -> Result<Self, PaperFetchError> {
        Ok(Self {
            db: sled::open(path)?,
        })
    }

    pub fn get(&self, query_hash: &str) -> Option<SearchResult> {
        self.db
            .get(query_hash)
            .ok()?
            .map(|bytes| bincode::deserialize(&bytes).ok())?
    }

    pub fn set(&self, query_hash: &str, result: &SearchResult) {
        let bytes = bincode::serialize(result).unwrap();
        self.db.insert(query_hash, bytes).ok();
    }
}
```

**Integration:**
Wrap service methods to check cache before calling providers.

---

### Provider Prioritization Logic

**Enhancement to MetaSearchService:**

```rust
impl MetaSearchService {
    fn prioritize_providers(&self, query: &SearchQuery) -> Vec<Arc<dyn PaperProvider>> {
        let mut providers = self.providers.clone();

        // Domain-specific boosting
        if query.query.contains("neural") || query.query.contains("machine learning") {
            // Boost arXiv and Semantic Scholar
            boost_provider(&mut providers, "arxiv", 20);
            boost_provider(&mut providers, "semantic_scholar", 15);
        }

        if query.search_type == SearchType::DOI {
            // CrossRef is authoritative for DOIs
            boost_provider(&mut providers, "crossref", 50);
        }

        providers.sort_by_key(|p| std::cmp::Reverse(p.priority()));
        providers
    }
}

fn boost_provider(providers: &mut [Arc<dyn PaperProvider>], name: &str, boost: u8) {
    // Temporarily increase priority (requires Arc<Mutex<u8>> in provider)
    // Or: sort with custom comparator
}
```

---

### Observability

**Add Metrics:**
```toml
[dependencies]
metrics = "0.21"
metrics-exporter-prometheus = "0.12"
```

**In Service Methods:**
```rust
use metrics::{counter, histogram};

async fn search_provider_with_resilience(...) {
    let start = std::time::Instant::now();

    counter!("search.requests", 1, "provider" => provider_name);

    let result = /* ... search logic ... */;

    histogram!("search.duration", start.elapsed().as_secs_f64(), "provider" => provider_name);

    result
}
```

Expose `/metrics` endpoint in API for Prometheus scraping.

---

## Deployment Considerations

### Configuration Management

**File:** `core/src/config.rs`

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub download_dir: PathBuf,
    pub cache_path: PathBuf,
    pub rate_limit_per_second: u32,
    pub max_concurrent_downloads: usize,
    pub provider_timeouts: HashMap<String, u64>,
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self, PaperFetchError> {
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents).map_err(|e| PaperFetchError::ConfigError(e.to_string()))
    }
}
```

**Usage:**
Pass config to service constructors instead of hardcoding values.

---

### Docker Deployment

**Dockerfile:**
```dockerfile
FROM rust:1.70 as builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY core core/
COPY api api/
RUN cargo build --release --bin paper-fetch-api

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /app/target/release/paper-fetch-api /usr/local/bin/
EXPOSE 3000
CMD ["paper-fetch-api"]
```

---

### Health Checks

**In API:**
```rust
#[oai(path = "/health", method = "get")]
async fn health(&self) -> Json<HealthResponse> {
    let status = self.search_service.health_check().await;

    Json(HealthResponse {
        status: if status.unhealthy_providers.is_empty() { "healthy" } else { "degraded" },
        providers: status,
    })
}
```

---

## Migration Path from Original

If you want to migrate the existing codebase:

1. **Create `core/` library** with models and traits (no breaking changes)
2. **Move provider implementations** to `core/src/providers/`
3. **Extract service logic** from MCP tools to `core/src/services/`
4. **Create `api/` wrapper** as new binary
5. **Keep MCP server** in `mcp/` (also wraps core)
6. **Gradually deprecate** MCP-specific code from core

---

## Key Rust Patterns Used

### Trait Objects for Polymorphism
```rust
Vec<Arc<dyn PaperProvider>>  // Runtime polymorphism
```

### Async Trait
```rust
#[async_trait]
trait PaperProvider { async fn search(...) }
```

### Arc for Shared Ownership
```rust
Arc<dyn SearchService>  // Thread-safe shared reference
```

### Result-Based Error Handling
```rust
Result<T, PaperFetchError>  // No exceptions
```

### Builder Pattern
```rust
reqwest::Client::builder().timeout(...).build()
```

---

## Timeline Summary

- **Week 1**: Models, errors, traits (foundations)
- **Week 2**: Resilience layer (rate limiting, circuit breakers, retries)
- **Week 3-4**: Provider implementations (arXiv, CrossRef, etc.)
- **Week 5**: Service implementations (meta-search, download, metadata)
- **Week 6**: API layer with Poem
- **Week 7**: Testing, refinement, documentation

**Total:** ~7 weeks for full implementation with testing.

---

## Known Limitations

1. **PDF Parsing**: Text extraction from PDFs is imperfect (especially multi-column)
2. **Provider Variability**: APIs change; expect breakage
3. **Rate Limits**: Aggressive searching will hit limits
4. **Legal Concerns**: Users must respect copyright (especially with fallback sources)

---

## Testing Strategy

### Unit Tests
- Individual provider implementations
- Error categorization logic
- Utility functions (caching, validation)

### Integration Tests
- Meta-search across providers
- Download with fallback
- End-to-end workflows

### Load Tests
- Concurrent searches (use `criterion` benchmarks)
- Rate limiter effectiveness
- Circuit breaker triggers

### Mocking
- Use `wiremock` to mock provider APIs
- Test resilience without hitting real services

---

## Resources

- **Hexagonal Architecture**: https://netflixtechblog.com/ready-for-changes-with-hexagonal-architecture-b315ec967749
- **Poem Framework**: https://github.com/poem-web/poem
- **Async Rust**: https://rust-lang.github.io/async-book/
- **Backoff Strategies**: https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/

---

## Final Architecture Diagram

```
┌───────────────────────────────────────────────────────┐
│              Transport Layer (Choose One)             │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────┐ │
│  │  Poem API   │  │  MCP Server  │  │     CLI      │ │
│  └─────────────┘  └──────────────┘  └──────────────┘ │
└───────────────────┬───────────────────────────────────┘
                    │
┌───────────────────▼───────────────────────────────────┐
│               Core Service Layer                      │
│  ┌──────────────────────────────────────────────────┐ │
│  │  SearchService │ DownloadService │ MetadataService│ │
│  └──────────────────────────────────────────────────┘ │
│  ┌──────────────────────────────────────────────────┐ │
│  │  Resilience: RateLimit │ CircuitBreaker │ Retry  │ │
│  └──────────────────────────────────────────────────┘ │
└───────────────────┬───────────────────────────────────┘
                    │
┌───────────────────▼───────────────────────────────────┐
│              Provider Adapters                        │
│  ┌────────┐ ┌─────────┐ ┌──────────────┐ ┌────────┐ │
│  │ arXiv  │ │CrossRef │ │SemanticScholar│ │  ...   │ │
│  └────────┘ └─────────┘ └──────────────┘ └────────┘ │
└───────────────────────────────────────────────────────┘
```

---

## Next Steps

Ready to start implementing? Let's begin with **Step 1: Define Core Types** by creating the models. When you're ready, I'll guide you through creating `core/src/models.rs` with all the necessary data structures!

**Lesson:** Clean architecture through trait-based abstraction enables transport-agnostic services while enforcing boundaries that make complex systems testable and maintainable.
