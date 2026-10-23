# Perf — repetition detector (rc.385)

## Detector cost

Both detectors fed and checked per word, as the stream does, over the
200-document sample (2,129,649 words; a fresh detector per 2,000-word
region). Release build, big, three rounds (`evidence/detector_bench.txt`).

| | old (n-gram counts) | new (tail periodicity) |
|---|---|---|
| throughput | 2.25–2.30 M words/s | **4.68–4.74 M words/s** |
| per word (feed + check) | 434–444 ns | **211–214 ns** |
| heap allocation in steady state | 0 once the window's vocabulary is interned | **0**, and none at all after construction |

For scale: the new end-to-end run produced 6,469 words in 306 s for one
document, about 21 words/s, so the detector's ~0.2 µs per word is about
0.0004 % of conversion time. The change is not about speed, but it is not
slower.
## Conversion, end to end

Same two PDFs, same models, same backend (bmb llama-swap `glm-ocr:latest`,
VLM concurrency 2, layout/table on big's CPU). Production's own glm_ocr
conversions ran on bmb concurrently (2 in flight, 0–2 of 6 slots free), so
wall times include backend contention and are indicative only.

| document | old | new |
|---|---|---|
| `10.1016_j.eurpsy.2012.03.001` (6 pp) | 343 s → **refused** (6 regions aborted) | 306 s → **stored**, 40,206 B |
| `10.1016_j.eplepsyres.2007.12.015` (6 pp) | 200 s → **refused** (14 regions aborted) | 174 s → **stored**, 36,461 B |

The new run was faster although it generated every region to completion
instead of aborting 20 of them. Backend contention dominates that
difference; the point is that no extra time was spent.

Resources, new server: peak RSS **3.99 GB** (VmHWM), 205 threads. This is
the 8-way ONNX layout + table pools, unchanged by this change; the
detector's share is ~1.5 KB per in-flight region.

## Cost of waiting for an unambiguous loop

A real loop is now aborted later: ~29 loop words into a 2-word loop
(was ~6), ~48 into a repeated sentence (was ~23), ~200 into a replayed
paragraph (was ~82). At GLM-OCR's per-region cap of ~8k tokens, that is
≤ ~2.5 % of a looping region's worst case, paid only on real loops.
