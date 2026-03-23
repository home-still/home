# Dev Journal: 2026-03-22 - Four Search Providers

**Walkthrough:** Guided walkthrough — user wrote all code, agent reviewed

## What We Did

Added four full `PaperProvider` implementations to the `paper` crate, bringing the total from 2 (arXiv, OpenAlex) to 6. Each follows the exact same pattern established by `openalex.rs`: config struct, response deserialization structs, provider struct with constructor, response-to-Paper conversion, URL builder, and `PaperProvider` trait impl (search, get_by_doi, priority, supported_search_types).

**Providers added:**
1. **Semantic Scholar** (priority 85) — 214M papers, returns OA PDF URLs via `openAccessPdf` field. API key via `x-api-key` header.
2. **Europe PMC** (priority 75) — 33M biomedical papers, PDF URLs from `fullTextUrlList`. No auth required. Uses cursor pagination mapped to offset model.
3. **CrossRef** (priority 70) — 150M+ DOI records, metadata authority. Polite pool via `mailto` param. TDM PDF links (often paywalled). Trickiest parsing: hyphenated JSON keys, nested `date-parts` arrays.
4. **CORE** (priority 50) — 309M metadata records, OA full text. Optional API key via Bearer token. Very slow rate limit (5 req/10s), primarily useful as fallback.

All four are wired into the `--provider all` aggregate fan-out alongside arXiv and OpenAlex.

## Bugs & Challenges

### Config deserialization failure on existing config file

**Symptom:** `missing field 'semantic_scholar' for key "default.providers"` when running with existing `~/.home-still/config.yaml`

**Root Cause:** `ProvidersConfig` had `#[derive(Default)]` but not `#[serde(default)]`. When the YAML file had a `providers:` section without the new keys, figment failed instead of falling back to defaults.

**Solution:** Added `#[serde(default)]` to `ProvidersConfig`.

**Lesson:** Any struct that can be partially specified in config needs `#[serde(default)]`, not just `#[derive(Default)]`.

### Semantic Scholar rate limit on larger requests

**Symptom:** `-n 100` immediately hit rate limit.

**Root Cause:** Without an API key, S2 shares a pool of 1000 req/s across all anonymous users. Under load, even single requests can fail.

**Solution:** Expected behavior. The `ResilientProvider` wrapper handles retries. Users should configure an API key for reliable use.

### CORE returns 429 without API key

**Symptom:** Any CORE search immediately rate-limited.

**Root Cause:** CORE's free unregistered tier is extremely restrictive (1 batch or 5 single requests per 10 seconds), and may require an API key for any access.

**Solution:** Added specific 401 handling with a helpful error message suggesting the user configure an API key. In aggregate mode, CORE errors are silently ignored — other providers fill the gap.

### Binary name confusion

**Symptom:** `cargo run -p paper` failed — no bin target.

**Root Cause:** The actual binary is `hs` (in `crates/hs`), which wraps `paper` as a subcommand. The `paper` crate is a library.

**Solution:** Build and run via `cargo build --release -p hs && ./target/release/hs paper search ...`

## Code Changes Summary

- `paper/src/config.rs`: Added `SemanticScholarConfig`, `EuropePmcConfig`, `CrossRefConfig`, `CoreConfig` structs. Updated `ProvidersConfig` with all four fields. Added `#[serde(default)]` to `ProvidersConfig`.
- `paper/src/providers/semantic_scholar.rs`: New file. Full `PaperProvider` impl. S2 API with `x-api-key` auth, year-only dates, `externalIds.DOI` extraction.
- `paper/src/providers/europe_pmc.rs`: New file. Full `PaperProvider` impl. Author string splitting, PDF URL filtering from `fullTextUrlList`, cursor pagination mapped to offset.
- `paper/src/providers/crossref.rs`: New file. Full `PaperProvider` impl. Two response shapes (search vs DOI lookup), `date-parts` nested array parsing, author given/family assembly, polite pool `mailto`.
- `paper/src/providers/core.rs`: New file. Full `PaperProvider` impl. Bearer token auth, 401 handling with helpful message, year-only dates.
- `paper/src/providers/mod.rs`: Added 4 `pub mod` lines.
- `paper/src/cli.rs`: Added `SemanticScholar`, `EuropePmc`, `CrossRef`, `Core` to `ProviderArg` enum.
- `paper/src/commands/paper.rs`: Added 4 standalone match arms in `make_provider()` plus 4 provider blocks in the `All` aggregate arm. Added imports for all 4 providers.

## Patterns Learned

- **Repeatable provider pattern**: Config struct -> response structs -> provider struct with new() -> conversion method -> URL builder -> PaperProvider trait impl. Highly mechanical after the first one.
- **serde rename for reserved words**: `#[serde(rename = "abstract")] abstract_text` for Rust keywords in JSON.
- **Nested date parsing**: CrossRef's `date-parts: [[2023, 5, 15]]` — use `Option<u32>` inner elements, default month/day to 1 when missing.
- **Two response shapes**: CrossRef search returns `{ message: { items: [...] } }` but DOI lookup returns `{ message: <work> }`. Separate response structs (`CrSearchResponse` vs `CrDoiResponse`) handle this cleanly.

## Next Session

- Consider adding `#[value(alias = "s2")]` and `#[value(alias = "epmc")]` CLI aliases for convenience
- Clean up minor warnings (unused `SortBy` import in europe_pmc, unused `mut` in core)
- Test download resolution chain with new providers in aggregate
- Consider whether CORE and CrossRef `api_key`/`mailto` should share config with the downloader's existing fields
