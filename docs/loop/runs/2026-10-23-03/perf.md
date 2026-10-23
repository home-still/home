# Perf — catch-up skips failed conversions (rc.387)

## `hs pipeline catch-up --dry-run`

| | rc.382 | rc.387 |
|---|---|---|
| wall time (live store, big) | 1.7 s | **6.8 s** |
| reads | papers + markdown listings | + every catalog row (18,000+ YAMLs, 24 in parallel) |

The extra ~5 s is the catalog read the stamps live in; catch-up runs by
hand, rarely. It stops the run if the catalog cannot be read, rather than
guessing which documents are new work.

## `system_status`

No new read: the stamped set comes from the catalog listing the snapshot
already takes for the failed-conversion counts. Without that listing,
Stalled is reported unknown instead of counting failures.

## What it saves the scribe tiers

A catch-up today queues **144** events instead of **2,152** — **2,008**
fewer conversions of documents already given up on. At bmb's measured
~15 documents an hour (the only working tier), that is ~134 hours of
conversion capacity not spent re-failing, and new downloads not queued
behind it. For scale, the 2026-10-21 run's 1,070 failure events put the
newest download ~80 hours back.
