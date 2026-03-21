# Dev Journal: 2026-03-16 - VLM Text Gap Diagnosis

**Session Duration:** ~30 minutes
**Walkthrough:** None

## What We Did

Built a Python diagnostic script (`scripts/diagnose_text_gap.py`) to analyze the per-page text score gap between VLM (sglang GLM-OCR) and ONNX (PaddleOCR v5) eval results on the full 628-page OmniDocBench English subset.

The region-aware scoring fix brought VLM text from 76.62 → 83.38, but a 3.98-point gap remains vs ONNX baseline (87.37). This script cross-references both eval JSONs with OmniDocBench metadata to identify exactly which pages and categories are responsible.

## Key Findings

### Distribution of Deltas (VLM text - ONNX text)

| Bucket | Pages | % |
|--------|-------|---|
| VLM >10pt worse | 149 | 23.7% |
| VLM 5-10pt worse | 38 | 6.1% |
| Within ±5pt | 334 | 53.2% |
| VLM 5-10pt better | 33 | 5.3% |
| VLM >10pt better | 74 | 11.8% |

The gap is **concentrated**: 53% of pages are within ±5pt (VLM is competitive), but the 24% that are >10pt worse drag the average down hard.

### Per-Category Breakdown

| Category | n | VLM avg | ONNX avg | Delta |
|----------|---|---------|----------|-------|
| newspaper | 77 | 71.56 | 82.17 | -10.62 |
| PPT2PDF | 119 | 70.85 | 79.32 | -8.47 |
| colorful_textbook | 53 | 76.35 | 84.31 | -7.97 |
| book | 92 | 84.73 | 87.64 | -2.90 |
| magazine | 78 | 90.95 | 92.69 | -1.74 |
| academic_literature | 129 | 94.37 | 95.17 | -0.80 |
| exam_paper | 79 | 91.33 | 89.41 | **+1.91** |

### Root Causes Identified

1. **Newspaper pages (-10.62):** Dense multi-column text. VLM struggles with reading order and dense layouts. These are the single biggest drag on the average (77 pages × 10.62pt gap).

2. **PPT2PDF pages (-8.47):** 119 pages, second-largest category. VLM seems to hallucinate or misread styled/colorful text on slides. Many PPT pages drop from 95-100 (ONNX) to 30-50 (VLM).

3. **Colorful textbooks (-7.97):** Similar to PPT — colored backgrounds and styled text hurt VLM OCR quality.

4. **Over-extraction:** 122 pages have hyp/ref ratio >2x, meaning VLM extracts text from regions that shouldn't be scored (figures, diagrams, watermarks). This inflates hypothesis length without improving NED.

5. **Zero under-extraction:** No pages with ratio <0.3, so VLM always produces plenty of text — the issue is quality, not missing content.

### Bright Spots

- **Exam papers (+1.91):** VLM actually beats ONNX here — handwritten/form-style content plays to VLM strengths.
- **Academic literature (-0.80):** Nearly tied, clean layouts work well for both.
- **74 pages (11.8%)** where VLM is >10pt better — these are real wins.

## Bugs & Challenges

No bugs encountered. The script was straightforward data analysis.

### Data Join Subtlety

**Issue:** `data_source` field is nested at `page_info.page_attribute.data_source`, not at the top level of OmniDocBench.json entries.

**Solution:** Read the metadata structure first, then built the correct accessor path.

## Code Changes Summary

- `scripts/diagnose_text_gap.py`: New standalone diagnostic script. Loads VLM + ONNX eval JSONs and OmniDocBench metadata, produces per-page delta table, per-category breakdown, distribution buckets, and extraction ratio outliers.

## Open Questions

- Should we route specific categories (newspaper, PPT) back to ONNX pipeline instead of VLM? A hybrid approach could recover most of the gap.
- The 122 over-extraction pages suggest VLM is reading text inside figures/diagrams — is this a layout detection issue (regions not filtered) or a VLM prompt issue?
- Would a confidence-based router (use ONNX OCR confidence signals to decide VLM vs ONNX per-page) work better than category-based routing?

## Session 2: Deep Diagnosis Scripts

**Duration:** ~30 minutes

### What We Did

Built two more diagnostic scripts to go deeper into the over-extraction vs quality question:

1. **`scripts/dump_worst_pages.py`** — Extracts per-category text breakdown from OmniDocBench.json for the 5 worst pages in newspaper, PPT, and colorful_textbook. Shows which `category_type` contributes text, flags caption vs official categories, quantifies non-scored categories that could cause over-extraction if VLM reads them.

2. **`scripts/diff_worst_pages.py`** — Parses `EVAL_DEBUG_DIR` output files (ref vs hyp saved by eval harness) and produces: line-level unified diffs, heuristic classification of extra hypothesis lines (figure labels, captions, body text, short fragments), per-page and global classification aggregates, and hyp/ref ratio distribution.

### Critical Finding: Newspaper Gap is Ordering, NOT Over-Extraction

The 5 worst newspaper pages have hyp/ref ratios of **1.00-1.07** — VLM extracts the *right amount* of text but scores only 22-27% vs ONNX's 83-97%. These are dense multi-column layouts with 40-52 text blocks and 10K-14K chars of reference.

**This means the problem is reading order scrambling**, not over-extraction. VLM reads columns incorrectly or merges adjacent columns, producing text with all the right words in the wrong order. NED scoring devastates this — even a perfect set of words in wrong order produces a terrible edit distance.

### Caption Scoring Mismatch is Negligible

Global analysis across all 1355 OmniDocBench pages:
- Pages with caption text: 560 / 1355
- Caption fraction of all reference text: **2.93%** (65K of 2.24M normalized chars)
- Since both ref and hyp include captions, they roughly cancel. Not the root cause.

### Colorful Textbook: Figure-Internal Text

Worst colorful_textbook pages have very short reference (130-1164 chars) from children's textbooks with large figures. VLM reads text inside figures/diagrams that isn't annotated as scoreable. This is an upstream layout classification issue.

### Non-Scored Categories on Worst Pages

Headers, page numbers, and footnotes exist but are tiny (40-60 chars vs 8K-14K reference). Not a factor.

## Open Questions

- **Reading order on newspaper pages**: Is GLM-Edge-V fundamentally bad at multi-column reading order, or is our prompting/chunking causing it? Need actual hypothesis text (requires EVAL_DEBUG_DIR run).
- **Over-extraction population**: The 122 pages with >2x ratio from diagnose_text_gap.py appear to be *different* pages than the worst-delta newspaper pages (which have ratio ~1.0). Two distinct failure modes.
- **Hybrid routing**: Category-based routing (ONNX for newspaper/PPT) would recover the gap on worst categories but VLM wins on exam_paper (+1.91). Need per-page ceiling analysis.

## Next Session

1. Run eval with `EVAL_DEBUG_DIR` on newspaper subset to capture actual hypothesis text:
   ```bash
   EVAL_DEBUG_DIR=/tmp/debug_text EVAL_DEBUG_THRESHOLD=100 \
     cargo run --release --bin eval_runner --features eval -- \
     --data-source newspaper --limit 77 --backend sglang \
     --openai-url http://localhost:30000 -o /tmp/debug_newspaper
   ```
2. Run `diff_worst_pages.py` on the output to see actual text diffs
3. Based on findings, decide: reading order fix (prompting/chunking) vs layout classification vs hybrid routing
