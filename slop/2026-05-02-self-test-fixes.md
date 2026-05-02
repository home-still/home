# 2026-05-02 self-test fixes (rc.314 → rc.314-self-test-fixes)

The 18:19Z self-test FAILed on 6 gates. This session resolved F1 + F2 + F6 + landed prevention fixes for the underlying causes. F3 / F4 / F5 deferred to BACKLOG P1-15 (build `hs catalog repair --apply` CLI).

## Verification

| Gate | Before | After |
|---|---|---|
| F1: `pipeline_drift` | 6 | **0** ≤ 3 ✓ |
| F2: `distill_scan_repetitions.flagged_count` | 19 | **0** ✓ |
| F6: citation-sort top-10 includes off-topic 1289-cite POI paper | yes | **no — all 10 hits are on-topic RAG papers** ✓ |

## Code changes

- `paper/src/aggregation/relevance.rs` — added `TITLE_PRESENCE_FLOOR = 0.5`. When fewer than half the query terms are in the title, the score is hard-capped just under `CITATION_SORT_MIN_RELEVANCE` (0.3), regardless of how strong the abstract match is. Stops abstract-only matches from being lifted to the top by citation count. Existing test `target_paper_survives_citation_floor_even_with_fewer_citations` still passes (and is now additionally validated by the floor).
- `paper/src/providers/downloader.rs` — added `MIN_PDF_BYTES=100` gate + replaced the "store unknown content-type as-is" fallthrough with a `Permanent(NotFound)` error. Stops 0-byte stubs and binary garbage (JPEG-as-PDF, gzipped HTML landing pages) from polluting `papers/`.
- `hs-common/src/html.rs` — extended `is_paywall_html` with patterns for Cambridge Core paywall chrome, PMC site chrome, and OpenAlex landing-page UI. All three guarded by `!has_article` so real papers that mention these strings (e.g. PMC-hosted articles) aren't false-positived. 4 new unit tests, all pass alongside the original 5.

## Operational cleanup (S3 + Qdrant)

- **F1 drift cleanup**: Deleted 2 zero-byte / unknown-binary stubs (`10.1007_s10803-019-04204-9` and `10.1080_00224490902747222`) that were dispatch-loop fodder before the new `paper_download` gate would have caught them.
- **F2 contamination purge**: Removed 19 contaminated docs from Qdrant (via `hs distill purge`) and S3 — papers + markdown + catalog rows. Three buckets:
  - 6 Cambridge Core paywall chrome (`10.1016_j.eurpsy.2018.11.001`, `10.1017_s0033291721004517`, `10.1017_s0140525x08004214`, `W2137554955`, `W2766194567`, `W2910571628`)
  - 7 PMC site chrome (`10.1002_14651858.cd011611.pub3`, `10.1002_aur.2351`, `10.1111_fare.12423`, `W2145141773`, `W2289414900`, `W4220850985`, `b381accf594b832487241752aac9e3ebe0f3c6e1`)
  - 6 OpenAlex landing pages (`W2548974248`, `W2915623326`, `W2950888501`, `W2953958347`, `W3216857757`, `W4294732694`)
  Plus `10.48550_arxiv.2312.10997` (the scanner-blind-spot VLM-poisoned doc) — purged Qdrant + deleted markdown but kept the source PDF for re-conversion when QC threshold is tuned.
- **`papers/.quarantine/` cleanup**: 16 binary-garbage files (119 bytes each, 129+ failed convert attempts before the catch-up timer was disabled) — full deletion (papers + catalog). Net effect on counters: `documents` 3553 → 3537, `corrupted_pdfs` 96 → 82, `catalog_entries` 3599 → 3585.

## Deploy

- `~/.local/bin/hs` → `hs.rc314-self-test-fixes` (x86_64 native, contains the relevance + downloader + html-pattern changes)
- `~/.local/bin/hs-mcp` → `hs-mcp.rc314-self-test-fixes` (same)
- `~/.local/bin/hs-scribe-server` → `hs-scribe-server.rc314-self-test-fixes` (same — paywall pattern matching is in hs-common::html, used by the scribe convert handler)
- `hs-serve-scribe` and `hs-serve-mcp` systemd units restarted via SIGTERM-the-child trick (sudo unavailable; killed PIDs 2011425 and 2395269; both respawned within 2 seconds reading the new symlink targets).

## Not deployed in this session

- **`big_mac` (192.168.1.111) scribe**: Apple Silicon Mac; needs a native build of `hs-scribe-server` since the cross-compiled aarch64-unknown-linux-gnu binary won't run on Darwin. Until then, the second scribe in the pool will accept paywall HTML the new patterns reject — partial coverage. Build natively on big_mac next session.
- **F3 (3387 missing `downloaded_at`), F4 (33 phantom catalog rows), F5 (22 stuck_convert)**: deferred per plan to BACKLOG P1-15 (build `hs catalog repair --apply` CLI). The MCP `catalog_repair` tool's seven scan directions exist as read-only logic in `crates/hs-mcp/src/main.rs:779-1017`; building the apply path is mechanical translation but ~300-500 LOC of CLI command + atomic-write paths + tests.

## BACKLOG entries added

- P0-17: `papers.ingested` publish failure leaves catalog stamped + pipeline stuck (the silent-warn that caused F1's stuck-convert).
- P0-18: `paper_download` accepted unknown content-type as PDF (partial fix landed; regression test follow-up tracked).
- P0-19: `distill_scan_repetitions` blind spot — measures truncation count not run length (re-prioritized; same root cause as the deferred P0-14 from rc.308 follow-ups).
