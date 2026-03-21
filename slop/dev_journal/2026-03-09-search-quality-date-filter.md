# Dev Journal: 2026-03-09 - Search Quality & Date Filter

**Session Duration:** ~3 hours
**Walkthrough:** Guided (no prove-first phase)

## What We Did

### CLI Flattening
Removed the redundant `paper paper search` nesting. Moved `Search`, `Get`, `Download` from `PaperAction` enum directly into `NounCmd`. Deleted `PaperAction` entirely. Now it's simply `paper search "query"`.

### Date Filter (`-d` flag)
Added `DateFilter` struct to `paper-core/src/models.rs` with a hand-rolled parser supporting four operators (`>`, `>=`, `<`, `<=`) and partial dates (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`). Internally stores inclusive `after` and exclusive `before` bounds.

Examples:
- `paper search "transformers" -d ">=2025"` — 2025 and later
- `paper search "transformers" -d ">2023 <2025"` — 2023 through 2024

Server-side filtering via arXiv's `submittedDate:[FROM TO]` parameter.

### Search Quality (Chunk 1 of Metasearch Research)
Based on research doc at `slop/research/2026-03-09-metasearch-ux-design.md`:

1. **Removed exact phrase matching** — was wrapping queries in quotes, which killed recall
2. **AND-joined multi-word queries** — `"neural networks"` becomes `all:neural AND all:networks`
3. **Added relevance sorting** — `sortBy=relevance&sortOrder=descending`
4. **DOI auto-detection** — queries starting with `10.` containing `/` route to `get_by_doi`
5. **arXiv ID auto-detection** — queries matching `NNNN.NNNNNvN` pattern route to `get_by_doi`

### Install Scripts & GitHub Pages
Set up `docs/install.sh` and `docs/install.ps1` for cross-platform `curl | sh` installs. Deployed via GitHub Pages with a custom workflow (`pages.yaml`) to avoid the legacy Jekyll builder trying to clone private submodules.

## Bugs & Challenges

### URL Double-Encoding of `+AND+`

**Symptom:** Date filter queries returned 0 results.

**Root Cause:** `Url::parse_with_params` URL-encodes the query string. Using `+AND+` in the search query caused it to become `%2BAND%2B`, which arXiv didn't understand.

**Solution:** Use spaces instead of `+`: `format!("{} AND submittedDate:[{} TO {}]", ...)`. The `Url::parse_with_params` encodes spaces as `+`, which arXiv interprets correctly.

**Lesson:** When building URLs with `Url::parse_with_params`, use literal spaces for arXiv API separators, not `+`.

### DOI Search Returning 0 Results

**Symptom:** Searching for a DOI like `10.1038/s44386-025-00033-2` returned no results.

**Root Cause:** arXiv doesn't index DOIs in their `all:` search field. DOI queries need to go through the `id_list` parameter (which `get_by_doi` uses via `SearchType::DOI`).

**Solution:** Added `looks_like_doi()` detection in `run_search` to auto-route DOI-shaped queries to `get_by_doi` instead of the search endpoint.

### DateFilter Parser: Multiple Implementation Issues

**Symptom:** Various compile errors during user's manual implementation.

**Issues encountered:**
- Type mismatch: `op` was `char` in some branches and `&str` in others
- Missing `<` and `<=` match arms
- Missing return/validation at end of `parse()`
- `end_of_period` function had match arms outside the match block
- Typo: `Reuslt` instead of `Result`

**Lesson:** When guiding users through complex parsers, break the implementation into smaller pieces — struct first, then one operator at a time, then validation.

### GitHub Pages 404

**Symptom:** Pages deployed but returned 404.

**Root Cause:** Legacy Jekyll builder was trying to clone a private submodule (`hs-style`). Even with `.nojekyll`, the legacy builder ran first and failed.

**Solution:** Created custom `pages.yaml` workflow and switched Pages source from "Deploy from a branch" to "GitHub Actions" in repo settings.

## Code Changes Summary

- `crates/paper-core/src/models.rs`: Added `DateFilter` struct with `parse()`, `parse_partial_date()`, `end_of_period()` helpers. Added `date_filter` field to `SearchQuery`.
- `crates/paper-core/src/providers/arxiv.rs`: Removed forced quotes, AND-joined terms, added relevance sorting, added `submittedDate` server-side filtering.
- `crates/paper/src/cli.rs`: Flattened `PaperAction` into `NounCmd`. Added `-d`/`--date` arg to Search and Download.
- `crates/paper/src/commands/paper.rs`: Split `run()` into `run_search`/`run_get`/`run_download`. Added DOI/arXiv ID auto-detection. Added `lookup_and_display` helper.
- `crates/paper/src/main.rs`: Updated match for flattened CLI.
- `.github/workflows/pages.yaml`: New GitHub Pages deployment workflow.
- `docs/install.sh`, `docs/install.ps1`: Cross-platform install scripts.

## Patterns Learned

- **`Url::parse_with_params` encoding**: Use spaces in query values, not `+`. The method handles encoding.
- **Auto-detection routing**: Check input shape before deciding which API path to use (DOI → lookup, arXiv ID → lookup, everything else → search).
- **Partial date expansion**: For `YYYY` → expand to Jan 1 or Dec 31 depending on operator. For `YYYY-MM` → expand to 1st or last day of month. Keeps the parser clean.

## Open Questions

- Should `looks_like_arxiv_id` handle old-style arXiv IDs (e.g., `hep-ph/0001234`)?
- Should date filter support bare years without operators (e.g., `paper search -d 2025` meaning `>=2025 <2026`)?

## Next Session

- **Chunk 2**: Better result formatting — compact multi-line output, `--abstract`/`-a` flag, keyword highlighting in titles
- **Chunk 3**: Sort options (`--sort` flag)
- **Chunk 4**: Add OpenAlex provider
- **Chunk 5**: Dedup pipeline
- **Chunk 6**: RRF ranking
- Close GitHub issue #1 (TLS UnknownIssuer) — may already be fixed by `rustls-tls-native-roots`
