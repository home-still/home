# paper

Academic paper meta-search library. Searches arXiv and OpenAlex with resilient request handling (rate limiting, circuit breakers, retries). Part of [home-still](../README.md).

## Usage

Used via the `hs` unified CLI:

```sh
hs paper search "transformer attention"
hs paper download "neural nets" -n 25
hs paper get --doi "10.48550/arXiv.2301.00001"
```

See the [top-level README](../README.md) for full examples and options.

## As a library

```rust
use paper::config::Config;
use paper::models::{SearchQuery, SearchType, SortBy};
```

## Architecture

```
CLI (clap)
  -> Services (SearchService, DownloadService)
    -> Resilience (rate limiter, circuit breaker, retry)
      -> Providers (arXiv, OpenAlex)
        -> Aggregation (dedup, merge, RRF ranking)
```

Ports-and-adapters pattern: providers implement the `PaperProvider` trait, wrapped by `ResilientProvider` for fault tolerance. `AggregateProvider` fans out to multiple providers, deduplicates by DOI + fuzzy title matching, and ranks with reciprocal rank fusion.

## Build & test

```sh
cargo check -p paper
cargo test -p paper
cargo build --release -p paper
```
