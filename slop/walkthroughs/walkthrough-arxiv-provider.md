# ArxivProvider Implementation Walkthrough

## Overview
We're implementing the first concrete provider for academic paper search. ArxivProvider will fetch papers from arXiv's free API, parsing Atom XML feeds and extracting metadata.

## Why This Architecture
- **Trait implementation**: ArxivProvider implements PaperProvider, ensuring interface compliance
- **XML parsing**: arXiv returns Atom feeds, requiring roxmltree for namespace-aware parsing
- **URL construction**: Search queries map to arXiv's query syntax (ti:, au:, all:)
- **Error mapping**: HTTP/parsing errors map to our PaperFetchError types

## Dependencies
Already in Cargo.toml:
- `roxmltree = "0.20"` - XML parsing
- `reqwest` - HTTP client
- `url = "2.0"` - URL manipulation

May need:
- `urlencoding = "2.1"` - Query parameter encoding

## File Structure
```
paper-fetch-core/src/
├── providers/
│   ├── mod.rs           # Re-exports ArxivProvider
│   └── arxiv.rs         # Implementation
```

## Implementation Steps

### Step 1: Module Structure
Create providers module and declare it in lib.rs.

**Pattern:**
```rust
// providers/mod.rs
pub mod arxiv;
```

**Action:** Create `paper-fetch-core/src/providers/mod.rs` with module declaration.

**Gotchas:** Don't forget to add `pub mod providers;` to lib.rs for IntelliSense.

---

### Step 2: ArxivProvider Struct
Define the provider struct with HTTP client and base URL.

**Pattern:**
```rust
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
}
```

**Action:** Add this struct to `providers/arxiv.rs`.

**Gotchas:** We have an unwrap here in Client::builder().build() - we'll fix that when we refactor to accept Config.

---

### Step 3: Implement name() and priority()
Start implementing the PaperProvider trait with simple methods.

**Pattern:**
```rust
use async_trait::async_trait;
use crate::ports::provider::PaperProvider;

#[async_trait]
impl PaperProvider for ArxivProvider {
    fn name(&self) -> &'static str {
        "arxiv"
    }

    fn priority(&self) -> u8 {
        80  // High priority for CS/Physics
    }
}
```

**Action:** Add trait impl block with these two methods.

---

### Step 4: supported_search_types()
Declare which search types arXiv supports.

**Pattern:**
```rust
fn supported_search_types(&self) -> Vec<SearchType> {
    vec![
        SearchType::Keywords,
        SearchType::Title,
        SearchType::Author,
    ]
}
```

**Action:** Add this method to the impl block.

**Why:** arXiv API supports ti:, au:, and all: query prefixes.

---

### Step 5: Query URL Construction
Build arXiv API URLs from SearchQuery.

**Pattern:**
```rust
impl ArxivProvider {
    fn build_query_url(&self, query: &SearchQuery) -> String {
        let search_prefix = match query.search_type {
            SearchType::Title => "ti:",
            SearchType::Author => "au:",
            SearchType::Keywords => "all:",
            _ => "all:",
        };

        format!(
            "{}?search_query={}\"{}\"&start={}&max_results={}",
            self.base_url,
            search_prefix,
            query.query,
            query.offset,
            query.max_results
        )
    }
}
```

**Action:** Add this helper method to the impl block (not the trait impl).

**Gotchas:** Need proper URL encoding for query.query - we'll add urlencoding crate if needed.

---

### Step 6: XML Namespace Setup
Parse Atom feeds with namespace awareness.

**Pattern:**
```rust
fn parse_atom_feed(&self, xml: &str) -> Result<Vec<Paper>, PaperFetchError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| PaperFetchError::ParseError(e.to_string()))?;

    let root = doc.root_element();
    let ns = "http://www.w3.org/2005/Atom";

    // Find entries with namespace
    let entries: Vec<_> = root
        .children()
        .filter(|n| n.has_tag_name((ns, "entry")))
        .collect();

    Ok(vec![])  // Placeholder
}
```

**Action:** Add this method, it will extract entries from the feed.

**Why:** Atom XML uses namespaces - must use tuple syntax `(namespace, tag)` for tag matching.

---

### Step 7: Extract Paper Fields
Parse individual entry elements into Paper structs.

**Pattern:**
```rust
fn extract_paper(&self, entry: roxmltree::Node, ns: &str) -> Result<Paper, PaperFetchError> {
    let id = entry
        .children()
        .find(|n| n.has_tag_name((ns, "id")))
        .and_then(|n| n.text())
        .ok_or_else(|| PaperFetchError::ParseError("Missing id".into()))?;

    let title = entry
        .children()
        .find(|n| n.has_tag_name((ns, "title")))
        .and_then(|n| n.text())
        .ok_or_else(|| PaperFetchError::ParseError("Missing title".into()))?;

    // Extract authors, abstract, published date, PDF link...

    Ok(Paper {
        id: id.to_string(),
        title: title.to_string(),
        authors: vec![],  // Placeholder
        abstract_text: None,
        publication_date: None,
        doi: None,
        download_url: None,
        source: "arxiv".to_string(),
    })
}
```

**Action:** Add this helper method for parsing individual entries.

---

### Step 8: Implement search()
Wire up the async search method.

**Pattern:**
```rust
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
```

**Action:** Add to PaperProvider impl block.

---

### Step 9: Basic Test
Verify URL construction works correctly.

**Pattern:**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SearchQuery, SearchType};

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
        assert!(url.contains("ti:"));
        assert!(url.contains("neural networks"));
    }
}
```

**Action:** Add tests module at bottom of arxiv.rs.

---

## Known Limitations
- No URL encoding yet (spaces in queries)
- Authors not fully extracted
- PDF URLs not extracted
- Dates not parsed
- No rate limiting integration
- Client builder has unwrap

## Testing Strategy
1. Unit test URL construction
2. Unit test XML parsing with sample data
3. Integration test (optional) hitting real API

## Resources
- arXiv API: https://arxiv.org/help/api
- roxmltree docs: https://docs.rs/roxmltree/latest/roxmltree/
