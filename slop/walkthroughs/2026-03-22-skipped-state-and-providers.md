# Walkthrough: Skipped State + Semantic Scholar Provider

**Date:** 2026-03-22
**Status:** Planning
**Checkpoint:** fb5466a1500bb243bf4d2fcabb9a3e01bd839fdf

## Goal

1. Show "already exists" for papers that are already downloaded instead of silently re-downloading
2. Add Semantic Scholar as a download resolver to improve success rate for papers Unpaywall can't find

## Acceptance Criteria

- [ ] Re-running download shows "already exists" with file size for previously downloaded papers
- [ ] Summary line shows skipped count: "6/10 succeeded, 2 skipped, 2 failed"
- [ ] `DownloadEvent::Skipped` variant exists and is handled in progress UI
- [ ] Skipped papers display with a distinct style (not red, not green — dim/grey)
- [ ] Semantic Scholar added as a DOI resolver in the download chain (after Unpaywall)
- [ ] `hs paper download` with Semantic Scholar resolves more papers than before

## Technical Approach

### Part A: Skipped State

The skip check already exists in `download_by_url` (returns `Ok(DownloadResult)` for existing files), but:
- It's silent — no way to distinguish "downloaded" from "skipped" in the UI
- `download_by_doi` doesn't check (it resolves a URL first, then calls `download_by_url`)

**Approach:** Add a `skipped` field to `DownloadResult`, a `Skipped` event, and a `finish_skipped` style.

### Part B: Semantic Scholar Provider

Not a search provider — just a DOI resolver added to the download chain in `PaperDownloader`. Semantic Scholar has 214M papers and returns `openAccessPdf.url` when available.

**API:** `GET https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=openAccessPdf`
**No auth required**, rate limit: 100 req/5min for unauthenticated.

### Files to Modify

- `paper/src/models.rs` — add `skipped: bool` to `DownloadResult`, add `skipped: Vec` to `BatchDownloadResult`
- `paper/src/services/download.rs` — add `Skipped` event, emit it, track skipped in batch results
- `paper/src/providers/downloader.rs` — set `skipped = true` in existing-file check, add Semantic Scholar resolver
- `hs-style/src/reporter.rs` — add `finish_skipped` to `StageHandle` trait
- `hs-style/src/tty_reporter.rs` — implement `finish_skipped` (dim style)
- `paper/src/commands/paper.rs` — handle `Skipped` event, update summary line

## Build Order

### Part A: Skipped State

1. **Model changes** — add `skipped` field to `DownloadResult`
2. **Event + service** — add `Skipped` variant, emit it in `download_batch`
3. **Style** — add `finish_skipped` to trait + TtyReporter
4. **Wire up** — handle in paper.rs, update summary

### Part B: Semantic Scholar Resolver

5. **Resolver method** — add `resolve_semantic_scholar` to `PaperDownloader`
6. **Chain it** — add to `download_by_doi` after Unpaywall, before error

## Steps

### Step 1: Add `skipped` to DownloadResult

**File:** `paper/src/models.rs`
**Status:** [ ] Not started

Add `pub skipped: bool` to `DownloadResult`. Update all construction sites to set `skipped: false` (normal downloads) or `skipped: true` (existing file).

### Step 2: Add Skipped event and tracking

**File:** `paper/src/services/download.rs`
**Status:** [ ] Not started

Add `DownloadEvent::Skipped { index, total, size_bytes }`. In `download_batch`, emit `Skipped` instead of `Completed` when `dr.skipped`. Track skipped separately in `BatchDownloadResult`.

### Step 3: Add `finish_skipped` to StageHandle

**Files:** `hs-style/src/reporter.rs`, `hs-style/src/tty_reporter.rs`
**Status:** [ ] Not started

Dim/grey style, no bar: `{prefix:WIDTH.dim} {msg}`

### Step 4: Handle Skipped in paper.rs

**File:** `paper/src/commands/paper.rs`
**Status:** [ ] Not started

Match `DownloadEvent::Skipped`, call `finish_skipped`. Update summary: "X succeeded, Y skipped, Z failed".

### Step 5: Add Semantic Scholar resolver

**File:** `paper/src/providers/downloader.rs`
**Status:** [ ] Not started

Add response types and `resolve_semantic_scholar` method. Insert in chain after Unpaywall.

### Step 6: Test

Run `hs paper download "autistic female"` twice — second run should show all as skipped. Check that Semantic Scholar resolves any of the 4 previously-failing DOIs.

---
*Plan created: 2026-03-22*
