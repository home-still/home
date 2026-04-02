# paper

Academic paper meta-search library for Rust. Searches 6 providers simultaneously with resilient request handling (rate limiting, circuit breakers, retries). Part of [home-still](../README.md).

## Usage

Used via the `hs` unified CLI:

```sh
hs paper search "transformer attention"           # search all providers
hs paper search --type author "Hinton" -n 5       # by author
hs paper search "diffusion" --date ">=2024" -a    # recent, with abstracts
hs paper download "neural nets" -n 25             # search + download
hs paper download --doi "10.48550/arXiv.2301.00001"  # single DOI
hs paper get --doi "10.1038/s41586-024-07487-w"   # lookup metadata
```

## Providers

| Provider | Coverage | DOI lookup | Download |
|---|---|---|---|
| arXiv | Preprints (physics, math, CS, bio) | Yes | Direct PDF |
| OpenAlex | 250M+ works, all disciplines | Yes | Via Unpaywall |
| Semantic Scholar | 200M+ papers, citation graphs | Yes | Via S2 |
| Europe PMC | Biomedical and life sciences | Yes | PMC OA |
| CrossRef | 147M+ DOI records | Yes | Publisher links |
| CORE | 300M+ open access papers | Yes | CORE repository |

When using `--provider all` (default), all providers are queried in parallel. Results are deduplicated by DOI + fuzzy title matching and ranked with reciprocal rank fusion.

## As a library

```rust
use paper::config::Config;
use paper::models::{SearchQuery, SearchType, SortBy};
use paper::providers::arxiv::ArxivProvider;
use paper::ports::provider::PaperProvider;
```

## Architecture

```
CLI (clap)
  -> Commands (search, download, get)
    -> Resilience (rate limiter, circuit breaker, retry)
      -> Providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE)
        -> Aggregation (dedup, merge, RRF ranking, quality filtering)
```

Ports-and-adapters pattern: providers implement the `PaperProvider` trait, wrapped by `ResilientProvider` for fault tolerance. `AggregateProvider` fans out to all providers with per-provider timeouts, deduplicates by DOI + fuzzy title matching, and ranks with reciprocal rank fusion enhanced by recency, citation, and multi-source boosts.

### Download pipeline

Downloads filter out papers without download URLs or DOIs before counting toward `-n`. The search over-requests by 50% to compensate. Downloads show an overall progress bar with per-file title-as-progress-bar coloring and ETA.

## Build & test

```sh
cargo check -p paper
cargo test -p paper
cargo build --release -p paper
```
