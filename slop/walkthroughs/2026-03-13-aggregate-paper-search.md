# Walkthrough: Aggregate Paper Search

**Date:** 2026-03-13
**Status:** In Progress
**Checkpoint:** ed0891653726bcc93314551e2345f16ecd8721bf

## Goal

Implement `--provider all` so `paper search` fans out to arXiv + OpenAlex concurrently, deduplicates results, merges metadata, and ranks via Reciprocal Rank Fusion.

## Acceptance Criteria

- [ ] `paper search "query" -p all` returns merged results from both providers
- [ ] Duplicate papers (same DOI or fuzzy title match) are merged into one entry
- [ ] RRF ranking scores papers found by multiple providers higher
- [ ] If one provider fails/times out, results from the other still appear
- [ ] `--json` output shows `"provider": "aggregate"`

## Technical Approach

### Architecture

`AggregateProvider` implements the existing `PaperProvider` trait, so it drops into `run_search()` with zero changes to the command layer. Internally it holds multiple resilient providers and orchestrates: fan-out → dedup → merge → rank.

### Key Decisions

- **`PaperProvider` over `SearchService`**: Drop-in replacement, no CLI changes needed
- **RRF k=60**: Standard constant from the original RRF paper
- **Fuzzy matching via `strsim`**: Already in deps, `normalized_levenshtein` with 0.85 threshold
- **Fan-out with `join_all`**: Simple, no task spawning needed

### Files to Create/Modify

- `crates/paper-core/src/aggregation/dedup.rs`: Fix bug + complete dedup
- `crates/paper-core/src/aggregation/merge.rs`: Field-level merge
- `crates/paper-core/src/aggregation/ranking.rs`: RRF scoring
- `crates/paper-core/src/services/search.rs`: AggregateProvider
- `crates/paper/src/commands/paper.rs`: Wire ProviderArg::All

## Build Order

1. **Dedup** — Foundation; everything else depends on grouping duplicates
2. **Merge** — Needs dedup groups to merge papers within each group
3. **Ranking** — Needs dedup groups + merged papers to score
4. **AggregateProvider** — Orchestrates the pipeline, needs all three above
5. **CLI wiring** — Plugs AggregateProvider into `make_provider()`

## Steps

### Step 1: Fix and Complete Dedup
**Status:** [ ] Not started

### Step 2: Implement Merge
**Status:** [ ] Not started

### Step 3: Implement Ranking
**Status:** [ ] Not started

### Step 4: Implement AggregateProvider
**Status:** [ ] Not started

### Step 5: Wire into CLI
**Status:** [ ] Not started

### Step 6: Test End-to-End
**Status:** [ ] Not started

---
*Plan created: 2026-03-13*
*User implementation started: 2026-03-13*
