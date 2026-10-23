# Research — new downloads wait days behind re-queued failures

**Date:** 2026-10-23
**Question:** why does a document downloaded today not convert, and why
does every document the scribe tiers handled in the last 6 hours fail?

---

## 1. The symptom, measured

| measurement (live, 2026-10-23 ~14:20 UTC) | value |
|---|---|
| documents the two scribe watchers received in 6 h | 88 |
| … terminated (`will not redeliver`) | 87 |
| … of those, **already stamped `conversion_failed` this morning** | **87 / 87** |
| their earlier `attempts` | 2 (34), 4 (21), 6 (21), 8 (9), 7 (1); one `pdf_parse_error` at **40** |

Every document converted in 6 hours was one the pipeline had already
given up on, and it gave up again.

## 2. The cause, read from the broker

The `PAPERS` stream is a work queue (`retention: workqueue`). Its
`scribe-workers` consumer delivers in order and was at stream sequence
~19,580 of 20,776, with **1,196 pending**. Every queued message was read
without consuming it (`nats stream get`, 1,197 messages) and classified
against a fresh copy of the catalog and the markdown listing:

| publisher | document | queued |
|---|---|---|
| `hs pipeline catch-up` (all at 2026-10-21 14:09 UTC, in 13 s) | stamped failed, **retryable** | **1,010** |
| `hs pipeline catch-up` | stamped failed, **bad source** (can never convert) | **60** |
| `hs pipeline catch-up` | already has markdown | 13 |
| `hs pipeline catch-up` | never converted, not stamped | **3** |
| `paper-download` | never converted, not stamped | **101** (oldest queued 2026-10-21 14:15) |
| `paper-download` / `inbox` | other | 10 |

`hs pipeline catch-up` re-publishes every source stem with no markdown
(`plan_catch_up`). A document whose conversion was given up on has no
markdown by definition, so catch-up re-queued **1,070** known failures —
including 60 whose sources can never convert — for the **3** documents
it exists for. In a FIFO queue they went ahead of every later download:
101 new papers have waited two days, and at the one working tier's ~15
documents an hour the newest waits ~80 hours.

And the dashboard sends the operator there: `hs status`'s Stalled row is
"unconverted − queued" (`count_unconverted_stems`, which counts stamped
stems) with the note "no markdown and not queued — run `hs pipeline
catch-up`". 829 of today's "stalled" are stamped failures.

## 3. What the literature says

- **Hohpe & Woolf, *Enterprise Integration Patterns*** (in the corpus): a
  message that makes its recipient fail "would just fail right away
  again" when delivered again; it has to be taken out of the channel —
  the reason invalid and dead messages get their own channels instead of
  going back on the main one.
- **Bass, Clements & Kazman, *Software Architecture in Practice*** (in
  the corpus, performance tactics): a scheduling policy is "a priority
  assignment and dispatching"; FIFO assigns priority by arrival alone. A
  bulk re-publish of known failures therefore takes priority over every
  new document that arrives after it.

The project already has the separate channel: a failure is stamped in
the catalog, shown in the Failed row (rc.386), and re-queued — by kind —
only by `hs pipeline reconvert-failed`. Catch-up is the one publisher
that ignores it.

## 4. The change

1. `hs pipeline catch-up` skips stems stamped `conversion_failed` and
   says how many, by kind, and which command recovers them.
2. `hs status`'s Stalled counts only unconverted stems with **no**
   failure stamp, so it points at catch-up only for documents catch-up
   will queue. `pipeline_drift` keeps counting stamped stems (its stated
   purpose).

## 5. Expected effect

A catch-up run today would publish the never-converted, unstamped stems
only — not the ~2,200 stamped ones it publishes now. The 1,070 known
failures already queued drain on their own (and the 1,010 retryable ones
become real recoveries once bmb runs rc.385); no new ones are added.
