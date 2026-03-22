# Walkthrough: Download Provider Fix + Unpaywall Resolver

**Date:** 2026-03-22
**Status:** Planning
**Checkpoint:** f9afd4542941a6728c31983a14fd96865a492c60

## Goal

Fix two bugs: (1) `download` only searches ArXiv while `search` searches all providers, and (2) `config init` doesn't prompt for Unpaywall email, so DOI-based downloads can't resolve non-ArXiv papers.

## Acceptance Criteria

- [ ] `hs paper download "autistic female"` finds papers from both ArXiv AND OpenAlex (not just 3)
- [ ] `hs config init --force` prompts for Unpaywall email before writing config
- [ ] Config template contains `unpaywall_email` marker that gets replaced when email is provided
- [ ] `download_by_doi()` falls back to Unpaywall API when DOI is not an ArXiv DOI
- [ ] Papers without download_url but with a DOI are resolved via Unpaywall (when configured)
- [ ] All existing tests pass, new tests cover config generation logic

## Technical Approach

### Architecture

Two independent bug fixes that together solve the same user problem ("why can't I download papers I can search for?"):

1. **Provider mismatch** (CLI layer): The `Download` command defaults to `--provider arxiv` while `Search` defaults to `--provider all`. One-line fix in `paper/src/cli.rs`.

2. **Missing Unpaywall resolver** (config + download layers): Even with `--provider all`, many OpenAlex papers lack a direct PDF URL. The Unpaywall API can resolve a DOI to an open-access PDF. This requires: config field, config template, init prompt, and the resolver itself.

### Key Decisions

- **Unpaywall email in config, not env var**: Follows the existing figment config pattern. Users set it once during `config init`.
- **DOI fallback chain (arXiv fast-path → Unpaywall → error)**: ArXiv DOIs are handled without an API call. Unpaywall is the general fallback. Clear error message if neither works.
- **Interactive prompt via stderr**: Uses `eprint!` so prompts work even when stdout is piped.

### Dependencies

- `serde::Deserialize` (already in paper's deps) for Unpaywall response types
- Unpaywall API (free, requires only an email)

### Files to Create/Modify

- `paper/src/config.rs`: Add `unpaywall_email` field to `DownloadConfig`
- `crates/hs/config/default.yaml`: Add unpaywall email comment marker
- `crates/hs/src/main.rs`: Add `prompt()`, `generate_config()`, email prompt during init, tests
- `paper/src/providers/downloader.rs`: Add Unpaywall types, resolver, DOI fallback chain
- `paper/src/cli.rs`: Change download default provider
- `paper/src/services/download.rs`: Fix progress bar index race

## Build Order

1. **Config schema** (`paper/src/config.rs`): Add the field first so the downloader can reference it
2. **Config template** (`default.yaml`): Add the marker the init code will search for
3. **Config init prompt** (`main.rs`): Wire up the interactive prompt + config generation
4. **Unpaywall resolver** (`downloader.rs`): Add types, resolver method, update DOI chain
5. **Download provider default** (`cli.rs`): One-line fix that ties everything together
6. **Progress bar fix** (`download.rs`): Use enumerated index instead of racy atomic load

## Anticipated Challenges

- **`serde::Deserialize` import**: The downloader file doesn't currently import it. Need to add the import for Unpaywall response types.
- **`unpaywall_email` field in struct but not constructor**: Adding the field to `PaperDownloader` requires updating `new()` to accept it from config.
- **Config template string matching**: `generate_config()` does exact string replacement, so the marker in `default.yaml` must match exactly.

## Steps (To Be Filled During Proof Phase)

[This section will be populated after we build and verify the implementation]

---
*Plan created: 2026-03-22*
*Implementation proven: [to be updated]*
*User implementation started: [to be updated]*
