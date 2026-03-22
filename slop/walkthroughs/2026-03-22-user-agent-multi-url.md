# Walkthrough: User-Agent Headers + Multiple Download URLs

**Date:** 2026-03-22
**Status:** In Progress
**Checkpoint:** 0765808ee94f3b2f99e85b6802707cc0b6319174

## Goal

Fix 403 Forbidden errors by adding User-Agent headers, and improve download success rate by trying all OpenAlex OA locations instead of just `best_oa_location`.

## Acceptance Criteria

- [ ] PaperDownloader client sends User-Agent and Accept headers
- [ ] Paper model supports multiple download URLs
- [ ] OpenAlex provider parses full `locations` array
- [ ] download_single tries each URL before falling back to DOI resolver chain
- [ ] Aggregation merge collects URLs from all sources
- [ ] Fewer 403 errors on `hs paper download "autistic female" -n 100`

## Steps

### Step 1: Add User-Agent headers to PaperDownloader
**File:** `paper/src/providers/downloader.rs`
**Status:** [ ] Not started

### Step 2: Change `download_url` to `download_urls: Vec<String>` on Paper
**File:** `paper/src/models.rs`
**Status:** [ ] Not started

### Step 3: Fix ArXiv provider for Vec
**File:** `paper/src/providers/arxiv.rs`
**Status:** [ ] Not started

### Step 4: Fix OpenAlex provider + parse locations array
**File:** `paper/src/providers/openalex.rs`
**Status:** [ ] Not started

### Step 5: Update download_single to try multiple URLs
**File:** `paper/src/services/download.rs`
**Status:** [ ] Not started

### Step 6: Update merge to collect all URLs
**File:** `paper/src/aggregation/merge.rs`
**Status:** [ ] Not started

### Step 7: Update output display
**File:** `paper/src/output.rs`
**Status:** [ ] Not started

---
*Plan created: 2026-03-22*
