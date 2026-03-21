# Architecture: paper-fetch

## What It Is

A meta-search tool that queries multiple free academic paper APIs and lets you download results. Standalone tool; part of the broader **Home Still** project — "a collection of free and open source tools to democratize knowledge acquisition, distillation, and comprehension." paper-fetch handles the acquisition phase; sibling projects handle what comes after.

## Home Still Ecosystem

```
Acquisition          Distillation          Comprehension
─────────────        ─────────────         ─────────────
paper-fetch          xycut-plus-plus       [planned]
(find/download)      (segment/parse)       (present/learn)
```

- **paper-fetch** — meta-search and download for academic papers (this project)
- **xycut-plus-plus** — Rust implementation of XYCut++ document segmentation algorithm
- **[planned]** — knowledge presentation and learning tools

The vision: paper-fetch finds and downloads papers, xycut-plus-plus segments them for extraction, future tools distill and present knowledge.

## Major Systems

```
┌─────────────────────────────────────────────────┐
│                 Transport Layer                  │
│         (Poem HTTP/MCP + Clap CLI)              │
│                  [planned]                       │
├─────────────────────────────────────────────────┤
│              Service Layer                       │
│    SearchService  ·  DownloadService            │
│              [traits defined]                    │
├─────────────────────────────────────────────────┤
│              Resilience Layer                    │
│  Rate Limiter · Circuit Breaker · Retry         │
│              [implemented]                       │
├─────────────────────────────────────────────────┤
│              Provider Layer                      │
│  arXiv [partial] · CrossRef · Semantic Scholar  │
│  CORE · Unpaywall · OpenAlex · PubMed           │
│  Project Gutenberg · Internet Archive           │
│         (any open, free, trustworthy source)    │
├─────────────────────────────────────────────────┤
│              Domain Layer                        │
│    Models · Error Handling · Config             │
│              [implemented]                       │
└─────────────────────────────────────────────────┘
```

### Domain Layer — `paper-fetch-core/src/`
Core data types and configuration. No dependencies on transport or external APIs.

- **`models.rs`** — `Paper`, `Author`, `SearchQuery`, `SearchResult`, `DownloadResult`, `PaperMetadata`
- **`error.rs`** — `PaperFetchError` enum with smart categorization (`Permanent`, `Transient`, `RateLimited`, `CircuitBreaker`) driving retry decisions
- **`config.rs`** — `Config` struct with builder pattern; holds paths and resilience settings

### Resilience Layer — `paper-fetch-core/src/resilience/`
Cross-cutting fault tolerance applied per-provider.

- **`rate_limiter.rs`** — Token bucket via `governor`; per-provider rate limits
- **`circuit_breaker.rs`** — Exponential backoff via `failsafe`; opens after N failures
- **`retry.rs`** — Exponential backoff + jitter via `backon`; only retries transient errors
- **`config.rs`** — `ResilienceConfig` (rps, backoff durations, thresholds, max attempts)

### Port Traits — `paper-fetch-core/src/ports/`
Hexagonal architecture boundaries. All async + `Send + Sync`.

- **`PaperProvider`** — Interface for academic sources (`search`, `get_by_doi`, `health_check`, `priority`)
- **`SearchService`** — Orchestrates multi-provider search (`search_all`, `search_provider`)
- **`DownloadService`** — Downloads by DOI or URL, returns `DownloadResult` with SHA256

### Provider Layer — `paper-fetch-core/src/providers/`
Concrete `PaperProvider` implementations. Each provider maps external API responses to domain `Paper` structs.

- **`arxiv.rs`** — arXiv Atom XML API (partial implementation)
- CrossRef, Semantic Scholar, CORE, Unpaywall, OpenAlex, PubMed, Project Gutenberg, Internet Archive — planned

### Transport Layer — `api/`
Not yet implemented. Planned:

- **Poem** — HTTP API + MCP/Skills/Agents integration
- **Clap** — CLI tool

## How Systems Talk

1. **User request** enters via transport (CLI or HTTP)
2. Transport calls **SearchService** or **DownloadService** traits
3. Service iterates **providers** by priority, applying **resilience** (rate limit → circuit breaker check → call → retry on transient error)
4. Providers hit external APIs, parse responses into domain **models**
5. Results aggregate and return through the stack

## Data Flow

**Search flow:**
```
User query → Transport → SearchService → [for each provider by priority]
  → RateLimiter.acquire() → CircuitBreaker.check() → Provider.search()
  → retry on transient error → aggregate results → return
```

**Download flow:**
```
DOI/URL → Transport → DownloadService → resolve URL
  → HTTP GET (streamed) → write to disk → compute SHA256 → return DownloadResult
```

## Conventions

- **Hexagonal architecture**: domain core has no transport dependencies; ports define boundaries; adapters implement them
- **Error-driven resilience**: error category determines retry behavior — permanent errors fail fast, transient errors retry, rate limits wait
- **Per-provider isolation**: each provider gets its own rate limiter, circuit breaker, and config
- **Async-first**: `tokio` runtime, `async-trait` for dyn dispatch
- **File layout**: `models.rs` for data, `error.rs` for errors, `ports/` for traits, `providers/` for implementations, `resilience/` for fault tolerance

## Deployment Constraints

- **Container-first**: Must build as a static binary suitable for minimal Docker/Podman images (e.g., `FROM scratch` or `distroless`)
- **K8s-friendly**: Stateless request handling; config via environment variables; health check endpoint for liveness/readiness probes; graceful shutdown on SIGTERM
- **No local state assumptions**: Download paths and cache paths configurable; no hardcoded filesystem assumptions

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `reqwest` | Async HTTP client |
| `tokio` | Async runtime |
| `serde` / `serde_json` | Serialization |
| `roxmltree` | XML parsing (Atom feeds) |
| `governor` | Rate limiting |
| `failsafe` | Circuit breaker |
| `backon` | Retry with backoff |
| `thiserror` | Error derive macros |
| `chrono` | Date/time |

## What's Built vs Planned

**Built:** Domain models, error handling, resilience layer, port traits, config system, architecture doc

**In progress:** arXiv provider (XML parsing, response mapping)

**Planned:** Additional providers (CrossRef, Semantic Scholar, CORE, Unpaywall, OpenAlex, PubMed, Project Gutenberg, Internet Archive), service implementations, Poem HTTP API, Clap CLI, caching layer, integration tests
