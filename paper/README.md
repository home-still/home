# paper

Academic paper search, lookup and download across several providers, with
rate limiting, circuit breakers and retries per provider, deduplication and
rank fusion of the merged results, and a guarded PDF download chain. A
workspace library used by `hs` and `hs-mcp`.

## User documentation

- [Find and download papers](../docs/how-to/find-and-download-papers.md)
- [Paper providers](../docs/reference/paper-providers.md): providers, query
  syntax, ranking, paging and the download chain.
- [`paper` configuration](../docs/reference/configuration.md#paper)
- [`hs paper` commands](../docs/reference/cli.md#hs-paper)

## Code map

```text
cli.rs, commands/       hs paper search | download | get
config.rs               the paper: section, validation, HOME_STILL_PAPER_* overrides
providers/              arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef, CORE
providers/set.rs        ProviderSet: every provider built once, with its limiter and breaker
providers/downloader.rs PaperDownloader: the ordered source chain, PDF gate, size cap, publish
providers/url_guard.rs  public-host check on every URL and redirect hop
stem.rs                 the one DOI-to-stem mapping
```

Providers implement the `PaperProvider` trait and are wrapped in
`ResilientProvider`. `AggregateProvider` queries them in parallel with
per-provider timeouts, deduplicates by DOI and fuzzy title, and ranks by
reciprocal rank fusion, content match and a log-scaled citation count.

A long-lived process builds one `ProviderSet` at startup and reuses it:
a set built per request shares no limiter or breaker state.

## Build and test

```sh
cargo check -p paper
cargo test -p paper
```

The Semantic Scholar citation-graph test reaches the live API and is ignored
by default:

```sh
cargo test -p paper -- --ignored citation_graph_live
```
