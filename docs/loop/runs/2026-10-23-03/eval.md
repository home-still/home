# Eval — catch-up never re-queues a failed conversion (rc.387)

Live stores, 2026-10-23, read-only throughout: catch-up only with
`--dry-run`, queue messages read with `nats stream get` (which does not
consume a work-queue message), the rc.387 MCP server on a spare loopback
port. Old = deployed rc.382 binaries, new = rc.387, run back to back.

## 1. What catch-up would queue

| | rc.382 | rc.387 |
|---|---|---|
| sources it would republish | **2,152** | **144** |
| failed conversions it skips (and says so) | 0 | **2,008** (1,888 retryable, 120 not) |
| pointer to recovery | none | `hs pipeline reconvert-failed --dry-run` |

144 + 2,008 = 2,152: nothing else changed. The 2,008 are exactly the
catalog's stamped stems with a convertible source and no markdown (the
offline count from the catalog copy: 2,009, one stamped in between).

## 2. What `hs status` calls Stalled

| | rc.382 | rc.387 |
|---|---|---|
| Stalled ("no markdown and not queued — run `hs pipeline catch-up`") | **888** | **0** |
| `pipeline_drift` (meant to include failures) | 2,144 | 2,145 (one new download) |
| Queued | 1,264 | 1,264 |

Is 0 true? The queue was read message by message (1,265 pending): it
holds exactly **144** stems that are unstamped and have no markdown —
the same 144 catch-up would queue. Every unconverted, unstamped document
is already queued, so nothing is stalled. The old 888 were all failed
conversions, and its note sent the operator to the command that re-queues
them ahead of new work.

## 3. Why it matters (the queue as found)

| publisher | document | queued |
|---|---|---|
| the 2026-10-21 catch-up | stamped failed, retryable | 1,010 |
| the 2026-10-21 catch-up | stamped failed, bad source | 60 |
| the 2026-10-21 catch-up | already converted / unconverted | 13 / 3 |
| `paper-download` | new | 101, the oldest queued 2026-10-21 14:15 |

All 87 documents the scribe watchers terminated in the 6 h before were
already stamped this morning (attempts 2–8; one `pdf_parse_error` at 40).
With rc.387 a catch-up adds 144 events instead of 2,152.

## 4. Unchanged

- RECALL@10 25/25 (`evidence/queries_after.txt`) — search is untouched.
- The already-queued 1,070 failure events are not touched (no
  destructive step); they drain, and the 1,010 retryable ones become real
  recoveries once bmb runs rc.385.

## 5. Tests

| test | guards |
|---|---|
| `catch_up_plans_only_convertible_sources_without_markdown` (extended) | a gap-rejected PDF and a paywall stub without markdown are skipped and reported with their reasons; Stalled's count equals what catch-up plans; drift still counts both |
| `counts_only_what_catch_up_would_queue` (extended) | `all` includes a stamped stem, `unstamped` does not |
