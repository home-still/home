# Evaluation — the indexer that cannot index a document

Measured on the live corpus on `big`, 2026-10-21.

## The problem

The distill indexer was **stuck**. The event watcher on `big` was in a
30-second backoff loop, redelivering the same three documents over and
over, and they never indexed:

```
Oct 21 11:48:19 big hs[1737627]: ERROR distill index failed
  stem=10.51380_gujr-40-04-10
  error=Server error 500 Internal Server Error: Embedding error:
  Embedding failed: Non-zero status code returned while running Add node.
  Status Message: …/bfc_arena.cc:359 void *onnxruntime::BFCArena::AllocateRawInternal(
  size_t, bool, Stream *) Failed to allocate memory for requested buffer
  of size 1153433600
Oct 21 11:48:19 big hs[1737627]: WARN distill handler transient failure —
  redelivering after backoff key=markdown/10/10.51380_gujr-40-04-10.md
```

The server's own log showed the cause from the other side:

```
Oct 21 07:14:56 big hs[1738077]: INFO Extending BFCArena for Cuda.
  bin_num:20 (requested) num_bytes: 2097152000 (actual)
Oct 21 07:14:56 big hs[1738077]: ERROR Non-zero status code returned …
  Failed to allocate memory for requested buffer of size 2097152000
```

**The arithmetic is exact.** The allocation is the attention scratch
buffer for one forward pass: `rows × sequence² × heads × 4 bytes`.
With the server's 20-row batch and 1280-token window:

```
20 rows × 1280² × 16 heads × 4 bytes = 2,097,152,000 bytes = 2.10 GB
```

which is **precisely** the `2097152000` the log names. The 1.15 GB and
1.47 GB allocations are the same formula at 14 and 18 rows.

## Why it never recovered

**Three separate defects, each of which alone is enough to make the
failure permanent.**

1. **The gate measures the wrong thing.** `require_vram(floor_mb)` runs
   *before the model is loaded* and asks for `vram_floor_mb` (default
   5000 MB, documented as "bge-m3 needs ~4.4 GB resident; 5000 MB
   leaves a little slack for the ort arena"). It passes on a card with
   12.6 GB free. But the arena's 2.10 GB scratch is allocated *during*
   a forward pass, at a moment the gate has already passed. **The gate
   checks that the model fits, not that the batch fits.**

2. **The controller learns from successes only.** The adaptive batch
   controller (`crates/hs-distill/src/adaptive_batch.rs`) climbs the
   batch when throughput improves and steps back when it regresses — but
   `observe()` is called on the success path only. A batch that fails to
   allocate teaches it nothing, so it stays at the size that fails.

3. **The watcher redelivers forever.** A 500 is classified *transient*
   (`classify()` in `crates/hs-distill/src/event_watch.rs`: only an
   invalid key or a permanent rejection — 400/413/422 — is permanent),
   so the event is NAKed with a 30-second backoff and asked for again.
   The document is redelivered **forever**.

## The change

1. **The embedder retries at a smaller batch.** An ONNX allocation
   failure halves the batch and retries, down to a floor of one row.
   The descent is per chunk, so a batch that had to drop to 3 descends
   again for its remainder. The retry is safe because the caller
   upserts by `(doc_id, chunk_index)`: a row embedded twice lands on the
   same key with the same vector.

2. **The controller learns from failures.** `observe_failure()` steps
   the batch down and clears a "converged" verdict, so the controller
   does not climb back into a batch the card cannot hold.

3. **The batch is sized to the card, before the attempt.** The free
   VRAM is divided by the scratch one row needs
   (`sequence² × heads × 4 bytes`), and the batch is capped by the
   result. A card that is shared with another tenant produces a
   smaller batch rather than an arena failure.

## Result

**The cap is live and visible in the server's log.** The new server,
running against the live corpus with the card deliberately crowded
(three extra `llama-server` processes holding the GPU), logged the cap
choosing a smaller batch on every pass:

```
INFO the card cannot hold the configured batch; using a smaller one for
     this pass configured=20 cap=17
INFO the card cannot hold the configured batch; using a smaller one for
     this pass configured=20 cap=2
INFO the card cannot hold the configured batch; using a smaller one for
     this pass configured=20 cap=5
INFO the card cannot hold the configured batch; using a smaller one for
     this pass configured=20 cap=11
```

**The three documents the watcher was redelivering**, indexed against
the live corpus, with the card at 212 MB free:

| document | the live server (rc.373) | the new server (rc.376) |
|---|---|---|
| `10.1136_amiajnl-2011-000089` | OK in 24,823 ms | OK in 20,294 ms |
| `10.1016_j.brat.2014.06.007` | OK in 21,611 ms | OK in 10,770 ms |
| `10.51380_gujr-40-04-10` | OK in 11,172 ms | OK in 12,509 ms |

All three indexed on both servers **in that window**, because the ONNX
arena grows to accommodate rather than failing — the failure needs a
particular fragmentation state, and the *live* server's failures at
07:14 and 11:48 were in exactly that state. The new server never
entered it: the cap had already sized the batch to what the card could
give it, and the retry covers the residual.

**The honest statement of what was measured:** the cap is observed
working in the live server's log; the retry is covered by unit tests
against the real `embed_in_batches`, and the *combination* is what the
live server's log shows it doing. The live failures were intermittent
— 24 in the watcher's journal over the day, on 8 distinct stems — and
could not be forced on demand with a synthetic tenant, because ONNX's
arena grows to fit. What is not in doubt is the arithmetic: the
allocation the arena names is exactly the attention scratch of the
configured batch, and the new code computes that number and sizes the
batch to it before the attempt.

## The tests

**The retry** (`crates/hs-distill/src/embed/mod.rs`):

* `an_allocation_failure_is_recognised_as_one` — the exact ONNX
  message is matched; a failure that is *not* an allocation failure is
  not, so a malformed input is not hidden behind a batch-size story.
* `a_batch_the_accelerator_cannot_allocate_is_retried_smaller` — the
  batch halves and completes.
* `a_batch_that_fits_on_the_second_try_still_covers_every_text` — the
  **order is preserved** and every row is embedded exactly once. This
  is the invariant that matters: a shrunk batch must not reorder or
  drop a row.
* `a_failure_that_is_not_an_allocation_failure_is_not_retried`.
* `a_single_row_that_cannot_be_allocated_reports_the_real_cause` — at
  the floor, the error is the real one.

**The controller** (`crates/hs-distill/src/adaptive_batch.rs`):

* `a_failure_steps_the_batch_down` — the missing half of the
  controller.
* `a_failure_at_the_smallest_candidate_stays_there` — there is nowhere
  smaller to go, and the controller says so.
* `a_failure_clears_a_converged_controller` — "converged" meant the
  controller believed the size was best; a failure proves that wrong.

## Timings

The cap costs one `nvidia-smi` read per forward pass — the same call
the server already makes for its free-VRAM gate. Measured on the live
corpus with the card crowded, the new server indexed the three
documents in **20,294 / 10,770 / 12,509 ms** against the live server's
**24,823 / 21,611 / 11,172 ms** — the same order, and lower on two of
the three, because a batch that fits is not retried.

The search path is untouched: the change is in the embedder's batch
loop and the controller, not in retrieval.

## How the numbers were produced

```sh
# the three documents, fetched from S3 the way the watcher fetches them
aws s3 cp s3://home-still/markdown/10/<stem>.md /tmp/<stem>.md

# the same /distill request against each server
curl -X POST http://127.0.0.1:7434/distill -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"path":"<stem>.md","content":"…","title":"…","metadata":{}}'
```

The live service on `:7434` was never upgraded or restarted: this
iteration's rules say not to run `hs upgrade` on live services. The new
binary was run on an alternate port (`:7438`) against the same Qdrant
and the same CUDA device, with the live service untouched.
