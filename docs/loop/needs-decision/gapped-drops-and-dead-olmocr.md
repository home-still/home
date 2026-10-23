# Conversion has stored nothing for 14 hours: deploy rc.385, and decide on the dropped documents

**Status:** open, needs the operator. Measured 2026-10-23 ~11:07 UTC on `big`.

## What is happening

| measurement (last 14 h, both scribe watchers) | value |
|---|---|
| documents ended `terminating (will not redeliver)` | **274** |
| … of them `gapped_conversion` | **266** |
| documents stored | **0** |

Two causes, both measured:

1. **The glm_ocr tier (bmb) converts, and every result is refused.** Its
   streaming repetition detector aborts ordinary prose, each abort makes
   the conversion gapped, and gapped is permanent. **Fixed in rc.385**
   (see `runs/2026-10-23-01/`): two documents production dropped this way
   convert 6/6 pages with it. bmb runs rc.382.
2. **The olmocr tier (big) has no backend** (BACKLOG P0-27). vLLM exits at
   startup, `RuntimeError: Engine core initialization failed`, with 14,077
   MB free against the ~15,718 MB `--gpu-memory-utilization 0.65` needs;
   the tier's gate admits at 13,500 MB. A third-party tenant held 1,093 MB
   at the time. Its four "converting" olmocr subprocesses were 13–29 min
   old: they cycle against the dead backend and then escalate to bmb,
   where cause 1 drops them. Restarting the scribe service does not fix
   this; the backend fails the same way.

## Decisions

### A. Deploy rc.385 to bmb (recommended, now)

`hs upgrade --pre --yes` on bmb, once the rc.385 release is green. Every
hour before that loses ~19 documents to the gapped rule. big's watchers
need no change for this (the detector runs in the scribe server). This
loop does not run `hs upgrade`.

### B. Recover the documents already dropped

They are in `papers/` with a `gapped_conversion` stamp and no markdown
(BACKLOG P0-28). Options:

1. **Re-publish their convert events after A** (recommended): enumerate by
   catalog stamp, dry-run first, then publish. A bulk re-conversion of a
   few hundred documents through bmb at ~2–5 min each.
2. Leave them; re-ingest by hand on demand.

### C. Make the olmocr tier start (P0-27)

1. **Lower vLLM's `--gpu-memory-utilization` to fit beside the embedder**
   (≈0.55 ⇒ ~13.3 GB) and set the gate to the same requirement, so the
   gate and the backend agree. Needs a check that olmocr's KV cache still
   fits its context.
2. Raise the gate to ~15,800 MB: the tier then refuses honestly, and
   everything goes to bmb (which, after A, works).
3. Code fix (the loop's next candidate): derive the gate from the
   backend's real requirement so they cannot drift.

Recommendation: A now, B.1 after A is verified on bmb, C.3 as the next
loop iteration with C.2 as the interim config.
