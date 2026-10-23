# Research — two items: one blocked, one shipped

**Date:** 2026-10-23

## A. First pick, blocked: the embedder's VRAM growth starves olmocr

The olmocr tier (big) has not converted since 2026-10-22 22:01 UTC. vLLM
needs ~15,718 MiB free to start; 13,137 MiB were free. The distill
embedder held **8,510 MiB** idle (6,462 the day before, no restart).

Measured with the real `OnnxEmbedder`, production defaults, 200 real
documents (2,465 chunks):

| CUDA arena | after load | after doc 1 | after 200 docs |
|---|---|---|---|
| default (power-of-two growth) | 3,386 MiB | 7,482 | 7,482 |
| `kSameAsRequested` | 2,474 | 5,556 | **13,056** (worse) |
| `gpu_mem_limit` 4 / 5 GiB | — | batch **fails** | — |

The arena never shrinks; growing by request fragments (STAlloc,
arXiv 2507.16274: fragmentation under interleaved allocation); a cap
fails batches because the limit error is not recognised as OOM; the real
fix (per-run arena shrinkage) is unreachable through fastembed's private
session. No in-repo change both bounds memory and keeps throughput, so it
went to `needs-decision/gpu-budget-olmocr-vs-embedder.md` with measured
options (restart distill + stop `:7433` → 16,505 MiB free; or
`idle_release_secs`, reload 2.5–4.2 s). Evidence:
`evidence/embedder_vram_*.txt`.

## B. Shipped: `hs upgrade` takes Qdrant down even when nothing changed

**Symptom.** `hs upgrade` runs `pull` → `down` → `up -d` on every managed
compose file. On the GPU host that is Qdrant: every upgrade stops the
vector store, so search is down until it reloads its collections, and the
post-upgrade health wait (90 s) has expired before it is back — BACKLOG
P0-25: the rc.380 upgrade reported `Timed out waiting for Qdrant` on a
healthy host after ~7 minutes of recovery.

**Measured** on a throwaway Qdrant (reflink copy of the live 7.3 GB
storage, port 16333, same image), the exact upgrade sequence, image
already current (pull changed nothing):

| run | pull | down | up | until `/healthz` 200 | search outage |
|---|---|---|---|---|---|
| 1 | 1.0 s | 1.4 s | 1.5 s | 41.9 s | **44.8 s** |
| 2 | 4.0 s | 3.9 s | 3.6 s | 59.5 s | **67.0 s** |

Production recorded ~7 minutes on 2026-10-22 (cold page cache, a busy
host). Either way the outage buys nothing when the image did not change:
the live container runs image `8a80c550…`, which is what the `latest` tag
resolves to after a pull.

**Literature** (in the corpus): Bass, Clements & Kazman, *Software
Architecture in Practice* — a rolling upgrade replaces instances "one at
a time", each old instance destroyed only after its replacement serves;
and the availability tactic *escalating restart* varies "the granularity
of the component(s) restarted", "minimizing the level of service
affected". Restarting a stateful store whose code did not change is the
opposite: maximal granularity for no change.

**Technique.** After `pull`, compare what is running with what the tag
now names: for each container of the compose project (`compose ps -q`),
the engine's `inspect {{.Image}}`; for each service image in the resolved
config (`compose config`), `image inspect {{.Id}}`. Recreate only if they
differ, a container is missing, or the service's health URL does not
answer — otherwise leave it running and say so.

**Expected effect.** An `hs upgrade` with an unchanged Qdrant image:
search outage 45–67 s (up to ~7 min) → 0, and no false "Timed out
waiting for Qdrant". A changed image still recreates, as before.
