# Walkthrough: Complete ArxivProvider

**Date:** 2026-03-03
**Status:** Planning
**Checkpoint:** f27498ee315e9e64234e2481c8f9839ee24c272a

## Goal

Complete the ArxivProvider so it can search arXiv's Atom API, parse full paper metadata (authors, abstract, dates, PDF URLs), and return properly structured results. Config loads from `~/.home-still/paper-fetch/config.yaml`. Unit tests prove it all works.

## Acceptance Criteria

- [ ] Config loads from `~/.home-still/paper-fetch/config.yaml` (with defaults if file missing)
- [ ] Config structs serialize/deserialize to YAML
- [ ] ArxivProvider reads timeout and base URL from config
- [ ] `search()` sends HTTP request and returns `SearchResult` with parsed papers
- [ ] `extract_paper()` extracts all fields: id, title, authors, abstract, publication_date, doi, download_url
- [ ] `get_by_doi()` searches arXiv by DOI and returns a single paper
- [ ] URL query parameters are properly encoded (spaces, special chars)
- [ ] Unit tests pass for URL construction, XML parsing, field extraction, and config loading
- [ ] `cargo build` succeeds with zero warnings
- [ ] `cargo test` passes all tests

## Technical Approach

### Architecture

```
~/.home-still/paper-fetch/config.yaml
    ↓ loads
Config (config.rs)
    ├── resilience: ResilienceConfig
    ├── download_path, cache_path
    └── providers: ProvidersConfig
            └── arxiv: ArxivConfig { timeout_secs, base_url }
                    ↓ injected into
              ArxivProvider (providers/arxiv.rs)
                  → reqwest::Client (HTTP)
                  → roxmltree (XML parsing)
                  → Paper, SearchResult (domain models)
```

### Key Decisions

- **Config path**: `~/.home-still/paper-fetch/config.yaml` — project-wide requirement; resolve `~` via `dirs::home_dir()`
- **Config file optional**: If the file doesn't exist, use defaults — first-run experience shouldn't require manual config
- **serde_yaml**: Add as dependency for YAML serialization/deserialization
- **dirs crate**: Add for portable home directory resolution (works on Linux, macOS, Windows)
- **Provider config**: Nest provider-specific settings under `providers.arxiv` in YAML
- **URL encoding**: Use the `url` crate's `Url::parse_with_params` instead of adding `urlencoding`
- **Author parsing**: arXiv nests `<name>` inside `<author>` elements — iterate children, not just text
- **PDF URL extraction**: Use `<link>` elements with `title="pdf"` attribute
- **Date parsing**: Use `chrono::NaiveDate::parse_from_str` on first 10 chars of ISO 8601 timestamp
- **Total results**: Parse `<opensearch:totalResults>` from OpenSearch namespace

### Dependencies

- `roxmltree` — already in Cargo.toml
- `reqwest` — already in Cargo.toml
- `url` — already in Cargo.toml
- `chrono` — already in Cargo.toml
- `serde` — already in Cargo.toml (with `derive` feature)
- **`serde_yaml`** — NEW, for YAML config file loading
- **`dirs`** — NEW, for portable `~` resolution

### Files to Create/Modify

- `paper-fetch-core/Cargo.toml`: Add `serde_yaml` and `dirs` dependencies
- `paper-fetch-core/src/config.rs`: Add serde derives, YAML loading, provider config, config path resolution
- `paper-fetch-core/src/resilience/config.rs`: Add serde derives
- `paper-fetch-core/src/providers/arxiv.rs`: Complete implementation, accept config

### Example config.yaml

```yaml
download_path: ~/Papers
cache_path: ~/.home-still/paper-fetch/cache

resilience:
  rate_limit_rps: 1
  cb_initial_backoff_secs: 10
  cb_max_backoff_secs: 60
  cb_failure_threshold: 3
  retry_max_attempts: 5
  retry_min_backoff_ms: 100
  retry_max_backoff_secs: 30

providers:
  arxiv:
    base_url: "http://export.arxiv.org/api/query"
    timeout_secs: 30
```

## Build Order

1. **Add dependencies**: `serde_yaml` + `dirs` to Cargo.toml — unblocks config work
2. **Serde derives on config structs**: Add `Serialize`/`Deserialize` to `ResilienceConfig` and `Config` — unblocks YAML round-tripping
3. **Provider config + YAML loading**: Add `ArxivConfig`, `ProvidersConfig`, `Config::load()` with path resolution — unblocks providers reading settings
4. **Fix existing ArxivProvider bugs**: Remove typo `{E`, fix `au;` → `au:` — unblocks compilation
5. **Update ArxivProvider to accept config**: Constructor takes `ArxivConfig` — uses configured timeout/base_url
6. **URL encoding**: Use `url` crate for proper query construction — unblocks correct API calls
7. **`extract_paper()`**: Full field extraction from XML — unblocks search returning real data
8. **`search()`**: Wire up HTTP call + parsing — the primary user-facing method
9. **`get_by_doi()`**: Reuse search with DOI query — secondary feature
10. **Total results parsing**: Extract `opensearch:totalResults` — makes pagination work
11. **Unit tests**: Config loading, URL construction, XML parsing, field extraction — proves it all works

## Anticipated Challenges

- **Duration serialization**: `std::time::Duration` doesn't serialize nicely to YAML — use `u64` fields (seconds/milliseconds) and convert
- **XML namespaces**: Atom uses `http://www.w3.org/2005/Atom`, OpenSearch uses `http://a9.com/-/spec/opensearch/1.1/` — must use tuple syntax `(ns, tag)` with roxmltree
- **arXiv ID format**: IDs look like `http://arxiv.org/abs/1234.5678v1` — extract just the `1234.5678v1` part for our `Paper.id`
- **Missing fields**: Some entries may lack DOI, abstract, or PDF link — all optional fields, use `Option`
- **Config file doesn't exist on first run**: `Config::load()` must return defaults gracefully, not error

## Steps (To Be Filled During Proof Phase)

[This section will be populated after we build and verify the implementation]

---
*Plan created: 2026-03-03*
*Implementation proven: [to be updated]*
*User implementation started: [to be updated]*
