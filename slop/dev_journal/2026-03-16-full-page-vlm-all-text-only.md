# Dev Journal: 2026-03-16 - Full-Page VLM for All Text-Only Pages

**Session Duration:** ~15 minutes
**Walkthrough:** None

## What We Did

Removed the `TEXT_HEAVY_THRESHOLD` gate (previously set to 15 text regions) from the full-page VLM routing in `processor.rs`. Now **all** text-only pages (no tables, no formulas) are routed through full-page VLM instead of per-region cropping.

### Motivation

After the text-heavy (>15 regions) optimization brought us to 85.45 overall (1.28 from SOTA), analysis showed PPT2PDF pages account for 67% of the remaining text gap (-4.14 of -6.20 total). PPT slides typically have 3-10 text regions — well below the threshold of 15 — so they never hit the full-page VLM path. The 40 PPT pages scoring <70 aren't a reading-order issue; per-region VLM crops produce wrong text on styled/colored slides. Full-page VLM handles these natively.

### The Change

Three surgical edits in `processor.rs`:

1. **Deleted** `TEXT_HEAVY_THRESHOLD` constant and its 4-line doc comment
2. **Removed** `text_count` variable computation (filter + count over bboxes)
3. **Simplified** condition from `!has_tables && !has_formulas && text_count > TEXT_HEAVY_THRESHOLD` → `!has_tables && !has_formulas`
4. **Updated** log message: "Text-only page (no tables/formulas) → full-page VLM"

## Bugs & Challenges

None — clean compile on first try. The change was purely subtractive (removing a gate).

## Code Changes Summary

- `pdf-mash/src/pipeline/processor.rs`: Removed TEXT_HEAVY_THRESHOLD constant, text_count variable, and threshold check. All text-only pages now route to full-page VLM.

## Expected Impact

- **Text**: 86.50 → improvement expected (PPT pages moving from per-region crops to full-page VLM)
- **TEDS**: 79.64 → unchanged (tables excluded from this path)
- **CDM**: 90.21 → unchanged (formulas excluded from this path)
- **Overall**: 85.45 → improvement expected

## Next Session

Run full 628-page eval to measure actual impact:
```bash
cargo build --release --bin eval_runner --features eval
cargo run --release --bin eval_runner --features eval -- \
  --backend sglang --openai-url http://localhost:8080 \
  -o /tmp/hybrid_all_textonly_628
```

Compare against threshold=15 baseline. Key metrics to watch: PPT2PDF text scores (currently avg 70.85), and whether any non-PPT text-only pages regress.
