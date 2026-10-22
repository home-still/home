# Evaluation — olmocr conversions keep their pages

**Date:** 2026-10-22 · **Version:** v0.0.1-rc.383

## 1. Before: the measured gap

| measurement (live corpus) | value |
|---|---|
| search hits with a `page` (80 hits, 8 queries) | 72.5 % |
| search hits **without** a `page` → link opens page 1 | **27.5 %** |
| olmocr conversions that are flat (seeded 100-row sample) | **39 / 39** |
| other converters that are flat (multi-page) | 0 of 32 layout/GLM rows with real pages* |

\* the 3 flat non-olmocr rows are single-page or HTML, where flat is correct.

## 2. End to end, same input, before vs after

A real 6-page PDF (`10.1007_s11920-010-0151-5`) was converted by the real
olmocr CLI on `big`'s GPU (109 s). Its real output (result JSONL + the
`--markdown` file) was then fed, unchanged, through the **real
`hs-scribe-server` binary over real HTTP** (`POST /scribe/stream`) by a
stand-in CLI that refuses any PDF but the captured one (sha256-checked).
The live GPU could not be used for the second pass: see §5.

| | rc.382 (deployed) | rc.383 |
|---|---|---|
| pages in the returned markdown | **1** | **6** |
| pages identical to olmocr's own spans | 0 / 6 | **6 / 6** |
| page separators | 0 | 5 |
| bytes | 33,602 | 33,632 (= 33,602 + 5×7 separators − 5 joiner newlines) |

The text has 95 non-ASCII characters (33,420 characters, 33,602 bytes),
so the 6/6 match also proves the spans are converted from character
positions, not read as bytes.

## 3. Downstream: the page every chunk gets

The markdowns from §2 were stamped exactly as `event_watch` stamps the
catalog (`compute_page_offsets` → `resolve_page_accounting` → keep the
map only if `offsets_trustworthy`) and chunked by the real
`hs_distill::chunker::chunk_markdown` with its default config:

| | rc.382 | rc.383 |
|---|---|---|
| `offsets_trustworthy` | false | **true** |
| page-map entries in the catalog | 0 | **6** |
| chunks | 10 | 11 |
| **chunks with a `page`** | **0 (0 %)** | **11 (100 %)** |
| chunks per page | — | 1, 2, 2, 2, 2, 2 |

Every chunk of an rc.383 olmocr conversion carries a page, so every
search hit from it carries `#page=N` on its source link. (One more chunk:
chunks no longer straddle a page boundary — the same rule the layout
pipeline's documents already follow.)

## 4. Regression checks

| check | result |
|---|---|
| fixed query set, RECALL@10 | **25/25 = 100 %** (before: 25/25) — no search code changed |
| median search latency | 230 ms (before: 236 ms) |
| `cargo test -p hs-scribe --features server` | 365 lib + integration suites, 0 failed |
| `cargo test --workspace --exclude hs-scribe` | 1,389 passed, 0 failed |
| clippy `-D warnings` (workspace; hs-scribe default, server, cuda, eval) | all clean |
| rustdoc | 30 warnings (unchanged baseline) |

New tests (`converter::olmocr_subprocess`): one PDF page per markdown
page end to end through a stand-in CLI; character-position spans across
multi-byte text; an empty page keeps its place; a trailing thematic break
does not add a page; spans for the wrong page count, with a gap, out of
order, past or short of the text are refused as `olmocr_incomplete_pages`;
a document with no text is refused; a missing or ambiguous result names
the results directory. The new end-to-end test fails on the old code by
construction (it returned the flat file: one page).

## 5. What blocked the live-GPU pass (logged, not fixed here)

1. **llama-swap was wedged**: since at least 16:00 every olmocr request
   ended `200` with an empty body after 25–37 min and no vLLM process
   existed. A user-unit restart cleared it (the documented fix).
2. **A cold start fails whole documents**: after the 60 s `ttl` unload,
   llama-swap proxied requests to a vLLM port not yet listening
   (`500 connection refused`); olmocr exhausted its retries in ~20 s and
   failed 6/6 pages.
3. **Issue #10, now with numbers**: vLLM refused to start —
   *"Free memory on device (14.53/23.61 GiB) … less than desired GPU
   memory utilization (0.65, 15.35 GiB)"* — while the scribe tier's VRAM
   gate admits work at a **13,500 MB** headroom. The gate is set below
   what the backend needs (~15,718 MB), so it admits documents that then
   fail.
