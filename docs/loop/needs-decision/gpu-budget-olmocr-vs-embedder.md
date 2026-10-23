# The olmocr tier cannot start: the embedder's memory arena only grows

**Status:** open — needs the operator (config/restart on a shared host) or
a decision on forking a shared crate. Measured 2026-10-23 on the GPU host
(RTX 3090, 24,576 MiB).

## What holds the card

| process | VRAM |
|---|---|
| distill server (bge-m3 embedder), idle | **8,510 MiB** (was 6,462 the day before, no restart) |
| idle scribe server on `:7433` (disabled unit, not in the pool, 0 conversions) | 2,340 MiB |
| free | 13,137 MiB |
| vLLM needs to start olmocr (`--gpu-memory-utilization 0.65` × 23.61 GiB) | **~15,718 MiB** |

Stopping the idle `:7433` server (the earlier recommendation, C.0 in
`gapped-drops-and-dead-olmocr.md`) is **no longer enough on its own**:
13,137 + 2,340 = 15,477 MiB < 15,718.

## Why the embedder grows (measured)

The real `OnnxEmbedder`, production defaults (max_length 1280, adaptive
batch), fed 200 real documents (2,465 chunks, 2.13 M words) as the server
indexes them; this process's VRAM after each document:

| CUDA arena setting | after load | after doc 1 | after 200 docs | chunks/s |
|---|---|---|---|---|
| default (`kNextPowerOfTwo`, unbounded) | 3,386 MiB | 7,482 | 7,482 | 16.5 |
| `kSameAsRequested` | 2,474 | 5,556 | **13,056** | 15.9 |
| `gpu_mem_limit` 4 GiB / 5 GiB | 3,382 / 2,870 | **fails** ("Available memory … is smaller than requested bytes") | — | — |

- ONNX Runtime's CUDA arena never returns memory. With the default it
  extends in power-of-two steps: the first document added exactly
  **4,096 MiB**. Production took one more step (+2,048 MiB) on a larger
  batch, hence 8,510.
- Growing by the requested amount is **worse** (fragmentation: freed
  blocks of one size do not serve requests of another — the effect
  STAlloc, arXiv 2507.16274, measures for interleaved allocation).
- A hard cap fails batches instead of shrinking them: the arena-limit
  error is not one `is_out_of_memory` recognises, so the existing
  halve-the-batch retry never runs.
- The clean fix, shrinking the arena after each run
  (`memory.enable_memory_arena_shrinkage`, an ORT *run* option), is not
  reachable: fastembed keeps its session `pub(crate)` and exposes no run
  options. That needs a fastembed fork or upstream change (shared crate).

## Options

1. **Restart the distill server, and stop `:7433`** (recommended now).
   Measured: a fresh embedder settles at ~7,482 MiB, so free becomes
   ≈ 13,137 + 2,340 + (8,510 − 7,482) = **16,505 MiB > 15,718**. Holds
   until the arena takes another power-of-two step (it did within a day).
2. **Set `embedding.idle_release_secs`** (existing, documented key) on the
   GPU host, e.g. 120. The embedder drops to 0 MiB when idle and reloads
   in **2.5–4.2 s** (measured load times above) on the next request; that
   cost lands on the first search or index after an idle period. With
   vLLM resident (15.7 GB) and `:7433` stopped, ~8.4 GB remains for a
   reload that needs ~7.5 GB — it fits. Durable, no code change.
3. **Lower vLLM's `--gpu-memory-utilization`** to ~0.55 (~13.3 GB) in
   `run-olmocr.sh`, and the tier's headroom with it. Needs a check that
   olmocr's 8,192-token context still fits its KV cache.
4. **Fork/patch fastembed** to pass a run option that shrinks the arena
   after each batch. The real fix for the growth; a shared-crate change.

Recommendation: 1 now (restores the olmocr tier today), then 2 as the
durable setting; 4 only if the reload latency of 2 proves a problem for
search.
