# paper

Academic paper meta-search library and CLI. Searches arXiv and OpenAlex with resilient request handling (rate limiting, circuit breakers, retries).

See the [top-level README](../README.md) for usage and examples.

## As a library

The `paper` crate exposes its search and download functionality for use by other crates:

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
       -> Aggregation (dedup, merge, ranking)
```

Ports-and-adapters pattern: providers implement the `PaperProvider` trait, wrapped by `ResilientProvider` for fault tolerance.

## Build & test

```sh
cargo check -p paper
cargo test -p paper
cargo build --release -p paper
```
