# UX design for a scholarly metasearch aggregator

**A blended single-result list with Reciprocal Rank Fusion, DOI-first deduplication, and progressive streaming creates the best experience across CLI and REST interfaces.** The core architectural insight from two decades of library discovery systems (EBSCOhost, Primo, Summon) is that users strongly prefer a unified ranked list over per-source tabs—but unlike those systems that pre-index content, a real-time fan-out aggregator must solve streaming, dedup, and ranking on the fly. The patterns below, drawn from production metasearch systems, federated search literature, and modern CLI design, are directly implementable in Rust.

---

## 1. Present a single blended list with source provenance badges

Every successful scholarly metasearch system converges on the same core UX: **one ranked list, not tabs per source.** EBSCOhost Discovery Service blends results from ~23,000 providers into a single relevance-ranked stream. ProQuest Summon's "match-and-merge" technology removes ~500 million duplicates at index time to present one unified list. Google Scholar shows one result per paper with zero source indicators. Semantic Scholar operates similarly on its 200M+ paper corpus.

The recommended pattern for a real-time aggregator sits between full transparency and full abstraction: a **blended single list with subtle source badges**. Each result card shows the paper's metadata once, with small colored pills indicating which sources contributed data (e.g., `[OpenAlex] [arXiv] [S2]`). This builds trust with researchers who care about coverage while keeping the interface clean for non-experts. BASE (Bielefeld Academic Search Engine) validates this approach—it shows content provider per result and offers "Refine by Content Provider" as a facet, serving 12,000+ sources without overwhelming users.

**Facets must be normalized across heterogeneous metadata schemas.** Build three tiers of facets: universal facets that every source provides (publication year, document type, open access status), best-effort facets normalized from source-specific taxonomies (map arXiv categories, PubMed MeSH terms, and OpenAlex concepts to a unified field-of-study taxonomy), and source-specific facets shown contextually (MeSH terms for biomedical queries). EDS was specifically praised for keeping "subject phrases" intact in facets rather than breaking them into individual words—a warning against naive tokenization.

**The source status strip is the key progressive disclosure element.** Show a compact horizontal indicator with six source icons transitioning through states: gray (queued) → pulsing (searching) → green checkmark with count → yellow warning (slow/timeout) → red X (failed). Below results, include an expandable "Sources searched" panel with per-source result counts and response times. For non-experts, this collapses to "Searching 6 academic databases…" → "Found 1,416 papers across 5 databases."

---

## 2. Stream results progressively in four phases

The psychological evidence is unambiguous: **showing partial results feels dramatically better than waiting for complete results.** Facebook found skeleton screens led to **300ms faster perceived load** versus spinners at identical actual speeds. Luke Wroblewski reported that switching from spinners to skeleton screens in the Polar app eliminated wait-time complaints despite no change in actual latency. Perplexity AI's most important UX discovery was that "users were more willing to wait for results if the product would display the intermediate progress"—which led directly to their step-by-step execution display.

Nielsen's three response-time thresholds (unchanged for 30+ years) anchor the design: **0.1s feels instantaneous, 1.0s maintains flow, 10s is the attention limit.** For a six-source fan-out, implement these four phases:

**Phase 1 (0–200ms): Immediate feedback.** Show skeleton result cards and the source status strip with all six sources in "searching" state. Never show a blank screen after query submission.

**Phase 2 (200ms–1.5s): First results appear.** As the 2–3 fastest sources respond (typically OpenAlex and Semantic Scholar at 200–600ms), perform initial RRF merge and DOI-based dedup, then render the first results. Skeleton cards transform smoothly into real result cards. Mark these as preliminary results.

**Phase 3 (1.5–3s): Progressive enrichment.** As remaining sources respond, merge new unique results into the existing list and enrich existing cards with additional metadata (citation counts, PDF links, MeSH terms). **Critical rule: never reorder results the user has already seen.** Only insert new results and update metadata on existing cards.

**Phase 4 (3–5s+): Handle the slow tail.** This is the hardest problem. When 5 of 6 sources have responded but one is still pending, use a two-tier timeout: a **soft timeout at 2 seconds** (render available results, mark the slow source as "still searching") and a **hard timeout at 5 seconds** (stop waiting, show what's available, offer a "Retry [source]" button). Optionally continue fetching in the background for up to 10 seconds and silently merge late results if they arrive. Ex Libris explicitly warns against blending third-party indexes with local searches because "slow responses from the external index will impact the response time for all results"—the entire reason discovery systems moved to pre-indexed architectures. Since this system does real-time federation, the timeout strategy is critical.

---

## 3. Deduplicate with DOI-first, then fuzzy title matching

**DOI matching is the gold standard and should be the primary dedup key.** About 50% of works in OpenAlex have DOIs. Crossref holds 147M+ records as the primary DOI registration agency. DOI matching is O(1) via HashMap lookup and eliminates the need for expensive string comparison for the majority of duplicates.

However, DOIs have a critical edge case: **preprints and published versions have different DOIs.** Since January 2022, arXiv assigns DOIs with prefix `10.48550/arXiv.YYMM.NNNNN`, while the journal version carries the publisher's DOI. The arXiv record *may* contain the published DOI in its `doi` metadata field, but this is author-dependent and often missing. The solution is to build a **DOI equivalence map**: query Semantic Scholar's `externalIds` field (which contains `ArXiv`, `DOI`, `PMID`, `PMCID` mappings) or leverage OpenAlex's `locations` array, which already links preprint and published versions under a single Work entity.

For records without DOIs, use **title fuzzy matching with blocking to avoid O(n²) comparisons**:

- **Preprocessing**: Lowercase, strip punctuation, collapse whitespace, remove leading articles ("the", "a", "an"), normalize Unicode via NFKD
- **Blocking**: Group records by (year, first three characters of first author's last name) to reduce comparison space
- **Similarity**: Jaro-Winkler distance (available in Rust's `strsim` crate) or longest common subsequence ratio
- **Thresholds from the literature**: title similarity ≥0.95 → auto-merge; ≥0.90 with same first author → auto-merge; ≥0.85 with same first author and year → auto-merge but flag; ≥0.75 with different year or authors → keep separate, link as "possibly related"

**Present dedup results using Google Scholar's "All X versions" pattern.** Show one unified card per paper with the best available metadata, plus a "Found in: OpenAlex • arXiv • Semantic Scholar" badge and an expandable "View all 3 sources & versions" link. This reduces visual clutter while preserving access to all versions—critical when the arXiv preprint has a free PDF but the journal version has richer metadata.

### Which fields to prefer from which source

The "ideal merged record" combines the best metadata from each source:

| Field | Best source | Rationale |
|-------|------------|-----------|
| Title, authors, publication date | **Crossref** > OpenAlex | Publisher-deposited, most authoritative |
| Abstract | **Semantic Scholar** > PubMed > OpenAlex | S2 parses PDFs; PubMed has structured abstracts |
| TLDR summary | **Semantic Scholar** (exclusive) | AI-generated ~20-word summaries |
| Citation count | **OpenAlex** > S2 > Crossref | Comparable to Scopus, broader coverage |
| Topics/concepts | **OpenAlex** (hierarchical taxonomy) | Domain → field → subfield → topic |
| MeSH terms | **PubMed** (exclusive) | Controlled biomedical vocabulary |
| Open access status | **OpenAlex** (via Unpaywall) | Integrated Unpaywall data |
| PDF URLs | **arXiv** > CORE > OpenAlex | arXiv guarantees preprint PDFs |
| Funding, publisher info | **Crossref** | Publisher-deposited metadata |
| Influence scores | **Semantic Scholar** (exclusive) | Influential citation count, SPECTER embeddings |

At ~600 records (100 per source × 6 sources), the entire dedup pipeline—DOI indexing, blocking, fuzzy matching, field-level merging, and RRF ranking—should complete in **under 50ms** in Rust, well within the Phase 2 window.

---

## 4. Rank with Reciprocal Rank Fusion as the foundation

**Reciprocal Rank Fusion (RRF) is the right algorithm for this system.** The core challenge is merging results from sources with incomparable relevance scores: OpenAlex returns a proprietary `relevance_score`, Semantic Scholar uses a neural ranker, Crossref does basic text matching, and arXiv provides **no relevance scores at all**. Score normalization techniques (min-max, z-score, CombSUM) fail when one source lacks scores entirely.

RRF sidesteps this entirely by working on **ranks, not scores**: `RRF_score(d) = Σ 1/(k + rank_r(d))` for all sources r containing document d, where k=60. Cormack et al. (SIGIR 2009) proved RRF "outperforms Condorcet and individual rank learning methods." It requires no training data, handles missing results gracefully, and takes ~20 lines of Rust to implement. Elasticsearch, OpenSearch, Azure AI Search, and Milvus all use RRF in production.

**Enhance RRF with three boost signals:**

```rust
fn final_score(rrf: f64, pub_year: u16, citations: u32, source_count: u32) -> f64 {
    let age = (2026 - pub_year) as f64;
    let recency = f64::exp(-0.5 * (age / 10.0).powi(2)); // Gaussian, scale=10yr
    let cite_boost = 1.0 + (1.0 + citations as f64).ln() * 0.1;
    let multi_source = (source_count as f64 / 3.0).min(1.0);
    
    rrf * 0.75 + recency * 0.10 + cite_boost * 0.05 + multi_source * 0.10
}
```

The **recency boost uses a gentle Gaussian decay** (scale=10 years) because academic papers remain relevant for decades—unlike news. Papers within 2 years score ~1.0; 5-year-old papers score ~0.94; 10-year-old papers ~0.78. The **citation boost is log-scaled** to avoid massive bias against new papers. The **multi-source bonus** rewards papers found across multiple sources (analogous to CombMNZ's "chorus effect"—documents ranked highly across multiple lists are likely relevant).

For explicit sort options, provide: "Sort by relevance" (default, using the combined formula), "Sort by date" (pure reverse chronological), "Sort by citations" (for finding seminal works), and date-range filtering ("Since 2020").

CORI and ReDDE, often cited in federated search literature, are **resource selection algorithms, not result merging algorithms**—they answer "which databases should we query?" Since this system always queries all six sources, skip them entirely.

---

## 5. Translate queries automatically with heuristic detection

**The default strategy is simple: send the raw query to all APIs.** Every scholarly API handles plain text reasonably well. For "machine learning for drug discovery", OpenAlex's built-in stemming and full-text search, Semantic Scholar's neural ranker, PubMed's Automatic Term Mapping (which silently expands to MeSH terms and synonyms), and Crossref's bibliographic matching all produce good results from the raw string. Implementing custom query expansion on top of these APIs risks reducing precision with no guaranteed recall improvement.

**Use heuristic detection for structured query types:**

- **DOI detected** (regex `^10\.\d{4,}/`): Route to direct DOI resolution endpoints on Crossref, OpenAlex, Semantic Scholar, and PubMed. Skip keyword search entirely.
- **arXiv ID detected** (`^\d{4}\.\d{4,}`): Resolve directly via arXiv, then cross-reference in OpenAlex and S2.
- **Author search** (two-three capitalized words, or explicit `author:` prefix): Translate to API-specific author syntax—`au:"Smith J"` for arXiv, `query.author=Smith` for Crossref, `AUTH:"Smith"` for Europe PMC.
- **Title search** (explicit `title:` prefix): Use field-specific endpoints—`ti:"exact title"` for arXiv, `filter=title.search:` for OpenAlex, `match_title=true` for Semantic Scholar.
- **Everything else**: Keyword/topic search, sent as raw text.

**Query transparency should default to hidden, with opt-in verbose mode.** PubMed's "Search Details" panel is the gold standard for query transparency—it shows exactly how queries are translated, which MeSH terms were added, and what synonyms were expanded. Model the verbose output on this:

```
Query: "machine learning drug discovery"
├─ OpenAlex:  search=machine learning drug discovery
├─ arXiv:     all:machine AND all:learning AND all:drug AND all:discovery
├─ S2:        query=machine learning drug discovery
├─ Crossref:  query=machine+learning+drug+discovery
├─ PubMed:    "machine learning"[MeSH] AND "Drug Discovery"[MeSH] (ATM expanded)
└─ CORE:      machine learning drug discovery
```

Always include `query_translations` in the API response metadata so programmatic users can inspect it. This is essential for systematic reviews where reproducibility matters.

**Don't build your own query expansion engine.** PubMed's ATM handles MeSH expansion, Europe PMC's `synonym=TRUE` parameter handles biomedical synonyms, and OpenAlex's Kstem filter handles stemming. Each API's native expansion is better tuned to its own corpus than any cross-API heuristic could be. The one exception: if a query appears biomedical (contains terms like "disease", "treatment", "clinical"), ensure PubMed and Europe PMC are queried and weight their results slightly higher.

---

## 6. Design the CLI around ripgrep's "smart defaults" philosophy

The best CLI search tools share a design philosophy: **the common case requires zero flags.** ripgrep is recursive by default, respects `.gitignore`, uses smart case sensitivity, and colorizes output—all without configuration. `fd` searches case-insensitively and skips hidden directories by default. The scholarly search CLI should follow suit: `scholar search "CRISPR gene editing"` should query all sources, deduplicate, rank by relevance, and display formatted results with no flags required.

### Result formatting

In TTY mode, display each result as a compact multi-line block:

```
 1. CRISPR-Cas9 gene editing in clinical trials: a review
    Smith J, Zhang Y, et al. · 2024 · Nature Reviews Genetics
    doi:10.1038/s41576-024-00001-2  📄 Open Access  📊 142 citations
    [OpenAlex] [arXiv] [Crossref]
```

Bold the title with keyword highlighting. Dim the author list (truncated with "et al." after 3 authors). Cyan for DOI (as an OSC 8 hyperlink in supported terminals). Colored source badges. Abstracts hidden by default, shown with `--abstract` or `-a` flag—terminal real estate is precious.

### Multi-source progress with indicatif

Use `indicatif`'s `MultiProgress` for per-source tracking, but prefer a **compact single-line status** over six separate progress bars:

```
Searching: OpenAlex ✓  arXiv ✓  Semantic Scholar ⏳  Crossref ✓  PubMed ⏳  CORE ✓  (4/6)
```

This is achievable with a single `ProgressBar` updated via `set_message()`. Use `enable_steady_tick(Duration::from_millis(80))` for the spinner animation. On completion, transition to:

```
Searched 5 of 6 databases · 1,892 results · 1,420 unique papers (0.8s)
```

All progress output goes to **stderr** so it doesn't interfere with piped stdout. Use `std::io::stdout().is_terminal()` (stable since Rust 1.70) to detect TTY and suppress colors/progress when piped.

### NDJSON streaming for --json mode

Design structured events for machine-consumable output:

```json
{"event":"search_started","query":"CRISPR","sources":["openalex","arxiv","semantic_scholar","crossref","pubmed","core"]}
{"event":"result","source":"arxiv","paper":{"title":"...","doi":"...","authors":["Smith J"],"year":2024}}
{"event":"source_complete","source":"crossref","count":42,"duration_ms":1500}
{"event":"dedup","action":"merged","canonical_doi":"10.1038/...","sources":["arxiv","openalex"]}
{"event":"search_complete","total_raw":180,"deduplicated":120,"duration_ms":4200}
```

This is fully composable with jq: `scholar search --json "CRISPR" | jq -r 'select(.event=="result") | .paper.title'`. Support `--format` with values `table` (default TTY), `json` (NDJSON), `csv`, `bibtex`, `ris`, and `csl-json` for direct reference manager integration.

### TTY-adaptive dual-mode behavior

| Feature | TTY (interactive) | Piped (non-interactive) |
|---------|-------------------|------------------------|
| Colors | ANSI via `console` crate | None (respect `NO_COLOR`) |
| Progress | Spinners on stderr | Silent |
| Format | Rich multi-line table | One-line-per-result or NDJSON |
| Pagination | Auto-pager via `less -FRX` | Full output stream |
| Hyperlinks | OSC 8 clickable DOIs | Plain text |

Override with `--color={auto,always,never}`, `--no-pager`, `--interactive/--no-interactive`. Auto-page when TTY output exceeds terminal height, using `less -FRX` (quit if one screen, raw ANSI colors, no termcap init).

### Recommended Rust crate stack

Use `console` (terminal abstraction, 183M+ downloads), `indicatif` (progress bars/spinners), `dialoguer` (interactive prompts including `FuzzySelect` for paper selection), `clap` 4.x (argument parsing), `serde_json` (serialization), and optionally `ratatui` for a full-screen `--interactive` TUI mode with preview panes and keyboard navigation.

---

## 7. Design the REST API with dual sync/stream modes

### Single endpoint, two response modes

Implement one route that checks `Accept: text/event-stream` or `mode=stream` to switch between SSE and synchronous JSON:

```
GET /v1/search?q=machine+learning&mode=sync&timeout=10s     → JSON
GET /v1/search?q=machine+learning&mode=stream                → SSE
```

**For SSE**, use axum's first-class `Sse` support with typed events. Fan out to sources with `tokio::spawn` per source, each sending results through `tokio::sync::mpsc` channels. The main handler merges channel outputs into the SSE stream using `futures::stream::select_all`. Send `KeepAlive` heartbeat comments to prevent proxy timeouts.

Key SSE event types: `search_started` (query echo, source list), `source_started`, `result` (individual paper with source attribution), `dedup_merge` (when duplicates are detected), `source_complete` (per-source stats), `source_error`, and `search_complete` (totals, timing). Model this on OpenAI's Responses API event design, which uses semantic event types with sequence numbering.

**For synchronous mode**, wrap the fan-out in `tokio::time::timeout`. Return whatever has completed when the timeout fires. The `timeout` parameter (default 10s, max 30s) lets consumers trade completeness for speed.

### Response schema with per-source status

The synchronous response should include a `meta` object with per-source status, inspired by Elasticsearch's shard-level reporting:

```json
{
  "meta": {
    "search_id": "srch_abc123",
    "total_results": 87,
    "deduplicated_count": 72,
    "completeness": 0.83,
    "elapsed_ms": 2340,
    "sources": {
      "openalex": {"status": "success", "count": 25, "total_available": 14532, "response_time_ms": 890},
      "arxiv": {"status": "timeout", "count": 0, "error": "timed out after 8000ms"}
    },
    "warnings": ["Results may be incomplete: arxiv timed out"],
    "next_cursor": "eyJvcGVuYWxleCI6..."
  },
  "results": [...]
}
```

**Always return HTTP 200 for partial success** with inline warnings—not 207 Multi-Status, which is WebDAV-specific and poorly supported by HTTP clients. Reserve 502/504 for complete failure of all sources. Include `X-Search-Partial: true` as a header for programmatic detection.

### Federated pagination with composite cursors

The deep paging problem is inherent to federated search: "page 2" requires re-querying all sources because results are interleaved by relevance. Solve this with **composite cursor tokens** that encode per-source pagination state:

```json
{
  "openalex": {"cursor": "IlsxNjA5...", "offset": 25},
  "arxiv": {"start": 25},
  "semantic_scholar": {"offset": 100},
  "crossref": {"offset": 25},
  "pubmed": {"retstart": 25},
  "core": {"offset": 25},
  "query_hash": "sha256_of_original_query"
}
```

Base64-encode this into an opaque `next_cursor` string. On subsequent requests, decode the cursor, re-query each source from its saved position, merge/dedup/rank, and return the next page. Cap maximum depth at 10,000 total results—beyond that, users should refine their query. This mirrors OpenAlex's own cursor pagination, which encodes sort position in a Base64 token.

### Timeout and circuit breaker architecture

Configure per-source timeouts (5s for fast sources like OpenAlex, 8s for slower ones like arXiv) and implement circuit breakers per source with three states: Closed (normal) → Open (fail-fast, skip source) → Half-Open (probe with single request). When a circuit opens, report `"status": "circuit_open"` in the response and omit that source. After a 30-second reset timeout, send one probe request. Track this with a simple `tokio::sync::RwLock<CircuitState>` per source—no need for complex distributed state.

---

## 8. Make it "just work" for everyone

**The design philosophy is: one command for the common case, progressive disclosure for power users.** `scholar search "machine learning drug discovery"` queries all sources, deduplicates, ranks by relevance, and displays formatted results. No flags, no configuration, no API keys required (for sources that don't need them). Advanced options layer on top: `--source openalex,arxiv`, `--year 2020-2025`, `--sort citations`, `--format bibtex`, `--verbose`.

### Sensible defaults that serve both audiences

Default to **25 deduplicated results** (matching Google Scholar's first page), **relevance sort**, **all sources**, **brief output** (no abstracts in CLI). Researchers who want more use flags; non-experts get useful results immediately. Show open access indicators and citation counts by default—both audiences find these valuable. Hide abstracts behind `--abstract` in CLI (real estate constraint) but include them in API JSON responses (programmatic consumers want everything).

### Graceful empty results handling

When zero results return: check for DOI/arXiv ID patterns that might have been treated as keywords ("Did you mean to look up DOI `10.xxxx/yyyy`?"). If field-specific syntax produced no results, automatically retry as a general keyword search. Use the available results from faster, broader sources (OpenAlex, Crossref) to suggest spelling corrections. Progressively relax the query: remove date filters, then try individual keywords to identify which term is problematic. PubMed's approach is exemplary—when ATM-mapped search returns zero results, it automatically retries with all-fields search and shows warnings about terms not found.

### Partial failure as a first-class concept

When 1–2 of 6 sources fail, show results from successful sources with a **brief, non-alarming notice**: "Results from 5 of 6 sources (arXiv temporarily unavailable)." When all sources fail, distinguish the failure mode: network error ("Check your internet connection"), API key issue ("Run `scholar config` to update keys"), rate limiting ("Retrying in 5 seconds…" with automatic exponential backoff), or genuinely no results ("No papers found—try broadening your search"). Implement one automatic retry with backoff for transient failures (5xx, timeout). If a source fails 3+ consecutive times in a session, open its circuit breaker and skip it for subsequent queries.

The goal is the Houston airport principle: people don't mind waiting if they feel productive. Show results streaming in, show which sources are still working, and let users interact with available results while the slow tail completes. The metasearch should feel like a single fast search engine that happens to cover everything—not like six separate searches stitched together.

## Conclusion

The critical architectural decisions are: **Reciprocal Rank Fusion** over score normalization (it handles arXiv's lack of scores and requires no training data), **DOI-first deduplication** with title fuzzy matching as fallback (covering the ~50% of records with DOIs instantly and the rest within 50ms), and **four-phase progressive streaming** (skeleton → fast results → merge → slow tail timeout). The REST API should use SSE with typed events for streaming and composite cursor tokens for pagination. The CLI should follow ripgrep's smart-defaults philosophy with TTY-adaptive behavior.

The deepest insight from library discovery systems' evolution is that federated real-time search was **abandoned** by major vendors (MetaLib, 360 Search) in favor of pre-indexed unified indexes precisely because of latency and inconsistency. This system is choosing the harder path—real-time federation—which means the streaming UX, timeout strategy, and dedup pipeline aren't nice-to-haves but existential requirements. Get the Phase 2 window (200ms–1.5s to first results) right, and the system feels magical. Miss it, and users will prefer Google Scholar every time.