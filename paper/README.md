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

`--provider` takes `all`, `arxiv`, `openalex`, `semantic_scholar` (or `s2`), `europe_pmc` (or `europmc`, `pmc`), `crossref`, `core`; clap's own spellings `semanticscholar` and `europepmc` work too.

### Query syntax and paging

- Plain multi-word text is searched as a phrase. On arXiv, `AND` / `OR` / `NOT` between terms are operators and `"quoted phrases"` stay together; two terms with no operator between them mean `AND`. An unclosed quote is an error.
- `--offset` pages by result offset. OpenAlex pages by page number, so the offset must be a multiple of the page size (`-n`); Europe PMC pages by cursor and cannot take an offset (an error when searched alone, left out of `--provider all` when the offset is not 0). Other providers take any offset. `--provider all` never reports a next offset.
- A DOI lookup (`paper get`, the `paper_get` MCP tool) accepts the bare DOI, `doi:` or a `https://doi.org/` URL, any case.

### Citation graph (MCP `paper_references`, `paper_citations`)

Both go through Semantic Scholar under the shared rate limiter and breaker (every page of a citing-set walk is one limited request). `paper_citations` takes `limit` (1–1000, default 100), `year_from` and `sort` (`year`, the default, or `citations`; anything else is an error). `year_from` is applied while the pages are fetched, so `limit` counts papers that pass it. `sort=citations` ranks the whole citing set Semantic Scholar will serve (its ceiling is about 9000 edges); `sort=year` orders only the first `limit` papers Semantic Scholar returns. `truncated: true` means more exist than were returned.

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

Guarantees of every download (`PaperDownloader`):

- **One storage identity.** `paper::stem` is the only place a stem is built: the lowercased bare DOI with `/` → `_` (`10.1016/J.RASD` and `https://doi.org/10.1016/j.rasd` are one paper), else the provider id with separators replaced. A paper found by search and the same paper requested by DOI land on one key. Stems pass `hs_common::validate_stem`.
- **PDF only, bounded.** The body must start with `%PDF-`; HTML landing pages, images and stubs are rejected and nothing is stored. The transfer is aborted once `paper.download.max_download_bytes` (default 256 MiB) is crossed; the remote `Content-Length` never sizes a buffer.
- **Public hosts only.** Only `http`/`https` URLs without credentials, never a loopback / private / link-local / metadata address, checked before the request, on every redirect hop, and on the addresses a hostname resolves to. (Residual: if `HTTP(S)_PROXY` is set the proxy resolves the target, so the hostname check does not apply.)
- **Honest failures.** The source chain is ordered (arXiv, MDPI, Unpaywall, PMC, then the providers). A storage/IO failure aborts at once with the real error; otherwise every source's outcome (`no copy` vs `failed`) is listed in the final error.

### Sharing providers

`providers::set::ProviderSet::new(&config)` builds every provider with its rate limiter and circuit breaker **once**; a long-lived process builds it at startup and reuses the `Arc`s (`provider(&arg)`, `download_resolvers()`, `references()`, `citations()`). Building a set per request shares no limiter/breaker state.

### Configuration

`paper.*` keys are validated when the config loads (zero intervals/timeouts/concurrency, bad base URLs, an API key on a plain-`http` URL are errors). Environment overrides join words and levels with `_`: `HOME_STILL_PAPER_DOWNLOAD_PATH`, `HOME_STILL_PAPER_DOWNLOAD_TIMEOUT_SECS`, `HOME_STILL_PAPER_PROVIDERS_SEMANTIC_SCHOLAR_API_KEY`; an unknown `HOME_STILL_PAPER_*` variable is an error. New keys: `download.max_download_bytes`, `providers.semantic_scholar.max_retry_after_secs` (cap on a `Retry-After` sleep, default 30).

## Build & test

```sh
cargo check -p paper
cargo test -p paper
cargo build --release -p paper
```
