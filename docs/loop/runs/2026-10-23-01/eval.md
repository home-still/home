# Eval — repetition detector: consecutive loops only (rc.385)

All numbers compare the rc.384 detector ("old") with rc.385 ("new") on the
same data. Throwaway harnesses (`fp_check`, `det_bench`) replayed both
implementations side by side; they are not committed.

## 1. False positives on accepted documents

200 seeded accepted documents (seed 20261023), 46,083 blocks. Each block
was fed word by word with a check after every word, as the stream does.

| converter | docs | old: docs with an abort | new: docs with an abort | aborted blocks old → new |
|---|---|---|---|---|
| glm_ocr | 43 | 31 (72.1 %) | **0 (0.0 %)** | 200 → 0 |
| olmocr | 83 | 80 (96.4 %) | **3 (3.6 %)** | 800 → 3 |
| scribe-vlm | 72 | 65 (90.3 %) | 21 (29.2 %) | 799 → 75 |
| unknown | 2 | 2 | 0 | 7 → 0 |

The new rule's 78 aborted blocks, classified:

| kind | blocks |
|---|---|
| verbatim loop already stored in the markdown | 63 |
| assembled HTML table markup (not a production input; tables reach the VLM per cell) | 8 |
| other (mostly loops reaching past the 60-word sample the classifier saw) | 7 |

Examples of stored loops it now flags (true positives in "accepted" text):
"to be used to be used to be used …", "The study was conducted in a
controlled environment." ×4. These are logged as P1-29.

## 2. Recall on loops built from real text

For every block of ≥ 40 words (14,124 per shape), the block's own text
followed by 2,000 words of a loop made from it. A catch counts only if it
fires after the real text ends. Latency is loop words fed before the
abort.

| loop shape | old | new |
|---|---|---|
| 2-word induction loop | 88.7 % (5.9 w) | **98.8 %** (29.2 w) |
| 3-word induction loop | 89.2 % (7.0 w) | **99.4 %** (28.7 w) |
| last sentence repeated | 88.3 % (22.9 w) | **95.6 %** (47.7 w) |
| paragraph replayed (≤ 120 w) | 88.4 % (81.6 w) | **94.3 %** (199.7 w) |

The old rule's lower recall is the false-positive defect seen from the
other side: it had already fired on the block's own text. The new rule's
misses are blocks whose own text already ends in a loop.

## 3. End to end on documents production dropped

Two 6-page PDFs that bmb's glm_ocr tier converted and then refused as
`gapped_conversion`, `terminating (will not redeliver)` on 2026-10-22.
Both runs used the same layout/table models on big (CPU, `use_cuda=false`,
as on bmb) and the same backend as production: bmb's llama-swap,
`glm-ocr:latest`, VLM concurrency 2.

| document | production (rc.382) | old (rc.382 server) | new (rc.385 server) |
|---|---|---|---|
| `10.1016_j.eurpsy.2012.03.001` | gapped, 6 regions, dropped | gapped, **6** regions, exit 1, 343 s | **converted**, 6 pages, 40,206 B, exit 0, 306 s |
| `10.1016_j.eplepsyres.2007.12.015` | gapped, 14 regions, dropped | gapped, **14** regions, exit 1, 200 s | **converted**, 6 pages, 36,461 B, exit 0, 174 s |

- The old server reproduced production's region counts exactly (6 and 14),
  so this is the production failure, not a lookalike. Its 18 logged aborts:
  15 `4-gram-cycle`, 2 `trigram-cycle`, 1 truncated in the log; abort points
  108–841 bytes into the region (median 513).
- The new server logged **0** aborts.
- No loop got through: the most back-to-back copies of any unit in either
  new output is **2** ("non epileptic attack disorder non epileptic attack
  disorder"). Paragraphs of the new output that the old rule would abort
  read "Normal data were analysed using independent t-tests. Categorical
  data were analysed using Chi-squared test", which is methods prose.
- Server logs, client logs and the output check are in `evidence/`. The
  converted markdown is not committed (publisher full text); its sizes and
  SHA-256 digests are in `evidence/after_outputs.sha256`.

## 4. Unit tests (in the repo)

`repetition_detector` tests, 12, all pass. They cover the calibration
corpus's loops (fire), the four passages the old rule aborted (do not
fire), short legitimate repetition (does not fire), the 128-word period
boundary, and an oracle that checks the incremental runs against a direct
scan at every word of four seeded streams. A counting allocator shows
`feed`/`check` allocate nothing.

## 5. Gates

| gate | result |
|---|---|
| `cargo +1.99.0 fmt --all --check` | clean |
| clippy `-D warnings`: workspace minus hs-scribe; hs-scribe default; hs-scribe `server,cuda,eval` (all `--all-targets`) | clean ×3 |
| workspace tests (`unshare -rn`, `lo` up) | 1,573 passed, 0 failed, 8 ignored |
| hs-scribe `--features server` tests (same) | 377 passed, 0 failed |
| rustdoc `--workspace --no-deps` | 30 warnings = baseline 30; none in the changed file. `-D warnings` would fail on these pre-existing ones (logged) |
| query set (`eval_queries.py --limit 10`) | RECALL@10 **25/25**, unchanged (search untouched) |

Note on the test gate: under a bare `unshare -rn` the loopback interface
is down, and 72 tests that bind a local HTTP server fail with
`NetworkUnreachable (os error 101)`. They pass with
`unshare -rn sh -c 'ip link set lo up && cargo test …'`.
