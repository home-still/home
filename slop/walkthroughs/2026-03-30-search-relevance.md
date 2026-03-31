# Walkthrough: Paper Search Relevance Improvements

**Date:** 2026-03-30
**Status:** In Progress
**Checkpoint:** 09547ae03fc0f08d1cd2f131bfdae88e6eb62da4

## Goal

Transform the paper search from a naive keyword aggregator into a relevance-aware metasearch engine using phrase-aware queries, quality filtering, content relevance scoring, weighted RRF, and citation signal blending.

## Steps

### Step 1: Phrase Detection Utility
- [ ] Create `paper/src/providers/query_utils.rs`
- [ ] Register module in `paper/src/providers/mod.rs`

### Step 2: arXiv Phrase-Aware Queries
- [ ] Edit `paper/src/providers/arxiv.rs`

### Step 3: Europe PMC + CORE Phrase Quoting
- [ ] Edit `paper/src/providers/europe_pmc.rs`
- [ ] Edit `paper/src/providers/core.rs`

### Step 4: Quality Gate Filter
- [ ] Create `paper/src/aggregation/quality.rs`
- [ ] Register module in `paper/src/aggregation/mod.rs`
- [ ] Wire into `paper/src/services/search.rs`

### Step 5: Content Relevance Scorer
- [ ] Create `paper/src/aggregation/relevance.rs`
- [ ] Register module in `paper/src/aggregation/mod.rs`

### Step 6: Weighted RRF + Hybrid Ranking
- [ ] Edit `paper/src/aggregation/ranking.rs`
- [ ] Wire query string through `paper/src/services/search.rs`

### Step 7: Min-Citations CLI Flag
- [ ] Edit `paper/src/models.rs`
- [ ] Edit `paper/src/cli.rs`
- [ ] Edit `paper/src/commands/paper.rs`
- [ ] Edit `paper/src/commands/mod.rs`
- [ ] Filter in `paper/src/services/search.rs`
