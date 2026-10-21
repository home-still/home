# Run 2026-10-21-08 — the indexer that cannot index a document

## Problem, measured

The distill indexer is **stuck**. On `big`, 2026-10-21, the event
watcher was in a 30-second backoff loop, redelivering the same three
documents over and over, and they never indexed:

```
Oct 21 11:48:19 big hs[1737627]: ERROR distill index failed
  stem=10.51380_gujr-40-04-10
  error=Server error 500 Internal Server Error: Embedding error: Embedding failed:
  Non-zero status code returned while running Add node.
  Name:'/0/auto_model/encoder/layer.0/attention/self/Add'
  Status Message: …/bfc_arena.cc:359 void *onnxruntime::BFCArena::AllocateRawInternal(
  size_t, bool, Stream *) Failed to allocate memory for requested buffer
  of size 1153433600
Oct 21 11:48:19 big hs[1737627]: WARN distill handler transient failure —
  redelivering after backoff key=markdown/10/10.51380_gujr-40-04-10.md
```

The server's own log shows the same thing from the other side:

```
Oct 21 07:14:56 big hs[1738077]: INFO Extending BFCArena for Cuda.
  bin_num:20 (requested) num_bytes: 2097152000 (actual)
  rounded_bytes:2097152000
Oct 21 07:14:56 big hs[1738077]: ERROR Non-zero status code returned …
  Failed to allocate memory for requested buffer of size 2097152000
```

**The arithmetic is exact.** The allocation is the *attention scratch
buffer* for one forward pass: `rows × sequence² × heads × 4 bytes`.
With the server's 20-row batch and 1280-token window:

```
20 rows × 1280² × 16 heads × 4 bytes = 2,097,152,000 bytes = 2.10 GB
```

which is **precisely** the `2097152000` the log names. The 1.15 GB and
1.47 GB allocations are the same formula at 14 and 18 rows. Nothing is
mysterious: the server asks the GPU for a 2.10 GB scratch buffer and the
card cannot give it.

**Why the card cannot give it.** The embedder's own resident footprint
is **7.5 GB** (the distill server's share of the card). The card is
24.6 GB. A 12 GB VLLM tenant was on it for part of the day. 7.5 + 12 +
2.1 = 21.6 GB, and the ONNX arena cannot get a contiguous 2.1 GB block
from what is left.

**The VRAM gate does not cover it.** `require_vram(floor_mb)` runs
*before the model is loaded* and asks for `vram_floor_mb` (default
**5000 MB**, documented as "bge-m3 needs ~4.4 GB resident; 5000 MB
leaves a little slack for the ort arena"). It passes at 12.6 GB free.
But the arena's 2.10 GB scratch is allocated *during* a forward pass, at
a moment the gate has already passed, and 5000 MB of slack was never
enough for it. **The gate measures the wrong thing**: it checks that
the *model* fits, not that the *batch* fits.

**And the failure is permanent for that batch size.** The adaptive
batch controller (`crates/hs-distill/src/adaptive_batch.rs`) climbs the
batch size when throughput improves and steps back when it regresses —
but it only ever learns from **successes**: `observe()` is called on
the success path only, so a batch that fails to allocate teaches it
nothing. The controller stays at the size that fails, and every
redelivery fails identically.

**One more thing the arithmetic revealed, and it is the reason the
fix has two parts.** The free VRAM is *not* what the arena needs: at
12.6 GB free, 126 rows would fit by the free-VRAM arithmetic, and yet
the arena could not get a **contiguous** 2.1 GB block, because the card
was fragmented by three other processes. So a free-VRAM cap alone would
not have prevented these failures. The cap is the cheap, preventive
half; the **retry** is the half that actually covers the residual, and
it is the half that must be there. (Measured 2026-10-21, with three
extra `llama-server` processes holding the card: the new server logged
the cap choosing `cap=17`, then `cap=2`, `cap=5`, `cap=11` — and
indexed the three documents the watcher was stuck on.)

**The consequence for a user.** The watcher classifies a 500 as
*transient* (`classify()` in `crates/hs-distill/src/event_watch.rs`:
only an invalid key or a permanent rejection — 400/413/422 — is
permanent), so it NAKs the event with a 30-second backoff and asks the
broker to redeliver. The document is redelivered **forever**. The
`hs status` dashboard shows `Queued: 2452` and the corpus stops growing
on the documents that hit this.

**And the corpus is not short of them.** Measured 2026-10-21: 8
distinct stems failed in the watcher's journal, 24 failures, and
`Queued: 2452` with `Embedded: 15922 docs` of 15,866 markdown files.

## Why this ranks first

Against the runner-up — **cross-encoder reranking**, the backlog's first
item, whose GPU blocker has now cleared — this wins on the user's
ranking of harm:

* A cross-encoder **reorders** results. It makes the *right* passage
  more likely to be first. Measured on the fixed query set, it moved
  the expected passage's rank on **4 queries better and 3 worse**, for
  a **5× latency increase** (162 ms → 776 ms median) and a new GPU
  service, a new model and a new failure mode. The mean rank of the
  expected passage went 1.60 → 1.28.
* The indexer failure means a document is **not in the library at
  all**. The user asks a question about it and gets nothing — not a
  worse ranking, but *no answer*. That is a tier above a reordering.

The cross-encoder's own prior art predicted the null: our sibling
crate's M37 measured deepening the rerank head at **+0.78 points, 95 %
CI [−3.14, +4.71]** — indistinguishable from zero. This iteration
measured the same null on home-still's corpus.

## The technique

Searched the corpus first, and it holds the answer twice.

**The retry-the-smaller-shape pattern.** *Designing Data-Intensive
Applications*, on exactly-once semantics: *"If something goes wrong
while processing a message, you can either give up (drop the message—
i.e., incur data loss) or try again. If you try again, there is the
risk that it actually succeeded the first time, but you just didn't
find out about the success, and so the message ends up being processed
twice."* The chapter's remedy is **idempotence**: arrange the
computation so the final effect is the same as if it had run once. The
distill index is already idempotent — it upserts by
`(doc_id, chunk_index)` — so **retrying at a smaller batch size is
safe**: the worst case is that the same chunks are written twice, to the
same keys, with the same vectors.

**The crash-instead-of-loop principle.** *A Philosophy of Software
Design*, "§10.8 Just crash?": *"there are certain errors that it's not
worth trying to handle. Typically, … the error is not going to go away,
and it is better to crash the application and let the supervisor restart
it than to limp along."* A batch size that cannot allocate will not
become allocatable by retrying it: the card is busy, and the *right*
response is to use less of it, not to ask again.

**So the fix is the standard one, and it is the shape every serving
system converges on: on an allocation failure, halve the batch and
retry, down to a floor; below the floor, fail loudly.**

## The change, in three parts

1. **The embedder retries at a smaller batch.** When ONNX reports a
   memory allocation failure, the batch is halved and the forward pass
   is retried, down to a floor of one row. The controller is told the
   batch failed, so it **steps down instead of staying at the size
   that fails** — the missing half of the adaptive controller (it
   learns from successes only today). The retry is safe because the
   index is idempotent.

2. **The gate measures the batch, not the model.** The VRAM floor is
   checked against what the *next* forward pass will actually
   allocate — the model plus the batch's attention scratch — so a card
   that cannot host the batch is refused **before** the attempt, with
   an error that names the holders (the same shape as the existing
   `gpu busy:` error).

3. **The watcher stops redelivering forever.** A document that cannot
   be indexed because the *card* is busy is transient and should be
   retried; a document that cannot be indexed because the *server* says
   no is permanent and should be terminated. The classification is
   made explicit rather than inferred from the status code, so the
   error says which it is.

**What this is not.** Not a change to the corpus, a re-embed, or a
purge. Not a CPU fallback — the project forbids one, and this does not
add one: the batch gets smaller, the model stays on the GPU. Not a
silently degraded substitute: a floor of one row is *the* batch that
fits, and below the floor the server says so.

## Expected effect

* the three documents that have been redelivering every 30 seconds
  index, and the queue drains;
* an embedding that cannot allocate at the configured batch size
  **completes** at a smaller one, instead of failing forever;
* the batch controller learns from failures, so it does not climb back
  into a batch the card cannot hold;
* the VRAM gate names the *batch* it is refusing, and the holders it
  is refusing for.
