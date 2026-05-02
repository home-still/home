# 2026-05-02 home-still recovery — distill back online; F2 blocked on stamp-write defect

## TL;DR

- **F1 closed.** Qdrant container `home-still_qdrant_1` had been down for 24h (clean SIGTERM, OOMKilled=false). `podman start` brought it back; distill `/health` flipped to 200 with `compute_device: Cuda`, `version: 0.0.1-rc.314`, `qdrant_version: 1.17.1`, `embed_model: bge-m3`, 107,030 chunks across 3,518 docs. CUDA stayed on; no shims.
- **F2 not closed.** Pipeline drift = 6 (threshold 3) is not draining despite scribe reporting active conversions. Root cause is **not** queue saturation — it's a separate defect logged as **P1-19**: scribe's `total_conversions` counter and `last_conversion_at` advance while neither markdown files nor catalog stamps land. F2 will not resolve until P1-19 does.
- **Catalog drift surfaced.** Dry-run `catalog_repair` found 27 phantoms, 21 stuck_convert, 20 md_path_drift, 3367 missing `downloaded_at` stamps. All blocked on the missing `hs catalog repair --apply` CLI (P1-15 + Operational TODO F7).

## Timeline

| When | What |
|---|---|
| 2026-05-01 11:47 UTC | Last successful convert+embed pair landed in catalog |
| 2026-05-01 12:26 UTC | Qdrant container exited 143 (clean SIGTERM). Trigger unattributable from journals — they've rotated. `podman-compose@home-still.service` is a label artifact, not a real systemd unit on this host. |
| 2026-05-01 → 2026-05-02 | Distill stayed up and reported unhealthy via `/health 500: qdrant unreachable` for 24h. |
| 2026-05-02 12:11 UTC | Self-test reports F1 (`distill instance unhealthy, version: ""`) and F2 (`pipeline_drift = 6 > 3`). |
| 2026-05-02 ~12:11 UTC | Diagnosis: H4 (Qdrant unreachable) confirmed via direct curl on `:7434/health`. Qdrant container "Exited 24h ago" identified via `podman ps -a`. |
| 2026-05-02 ~12:13 UTC | `podman start home-still_qdrant_1`. Qdrant healthz passes. Distill `/health` 200 with full payload. |
| 2026-05-02 12:13–12:18 UTC | Re-measured `system_status` twice; `pipeline_drift` stayed at 6, `markdown` count unchanged at 3547. `scribe_health.total_conversions = 113`, `last_conversion_at` 90s before probe — but no new catalog activity. |

## What was fixed

Bringing back Qdrant fixed F1 entirely:

- `system_status.distill_instances[0]`: `healthy: true`, `version: 0.0.1-rc.314`, `compute_device: Cuda`, `embed_model: bge-m3`, `collection: academic_papers`.
- `system_status.qdrant`: non-null rollup, `qdrant_version: 1.17.1`.
- `distill_status` returns populated `health` and `status` (was `{null, null}`).

The plan's "no CPU fallback" rule held — `compute_device` returned cleanly as `Cuda`. No config edits, no shims.

## What didn't drain — and why

`pipeline_drift = documents − markdown − in_flight = 3563 − 3547 − 10 = 6`. With both scribe instances saturated and `inbox_pending: 0`, the natural-drain hypothesis from the plan said this should fall to ≤ 3 once one or two slots cycled.

It didn't. Across 5 min between two `system_status` probes:

- `markdown` count: 3547 → 3547 (no new files)
- `pipeline_drift`: 6 → 6
- `corrupted_pdfs`: 101 → 101 (no new failure stamps either)
- `catalog_recent` (with `include_repaired=true`): most recent activity 2026-05-01T11:47, no rows after Qdrant went down

But scribe says it's working:

- `scribe_health.total_conversions: 113` (since the scribe restart)
- `scribe_health.last_conversion_at: 2026-05-02T12:16:07Z` (90s before the probe)
- `gpu_utilization_pct: 53`, GPU memory 8938 MiB (5438 MiB owned by `hs-distill-server`, 3398 MiB by `llama-server`)

So scribe ticks the counter and updates `last_conversion_at`, but the convert-cycle (markdown write + catalog stamp + history row) does not complete. Either the counter ticks at acceptance rather than completion, or the write/stamp section of the convert handler is silently failing post-rc.311. **Pipeline drift cannot drain in this state.**

This is logged as **P1-19** in `BACKLOG.md`. Until that defect is fixed, the F2 gate is structural, not transient.

## Catalog drift snapshot (from dry-run `catalog_repair`)

| Scan | Count | Apply path |
|---|---|---|
| `disk_no_catalog` | 0 | n/a |
| `catalog_no_markdown` | 0 | n/a |
| `catalog_no_source` (phantoms) | 27 | **missing CLI** — `hs catalog purge` (P1-15) |
| `flag_drift` (would_backfill_conversion) | 21 | **missing CLI** — `hs catalog repair --apply` |
| `flag_drift` (would_backfill_downloaded_at) | 3367 | **missing CLI** — also wants P2-15 row-synthesis fix |
| `flag_drift_resync` | 0 | n/a |
| `md_path_drift` | 20 | **missing CLI** |
| `stuck_convert` | 21 (15 PDF + 6 HTML) | **missing CLI** — partial overlap with md_path_drift |
| `embedding stamp drift` (the only thing `hs distill reconcile --fix-stamps` actually fixes) | 0 | clean ✓ |

Read of `crates/hs/src/distill_cmd.rs:1235` confirms `hs distill reconcile` only handles two narrow classifications: `StampMissing` (markdown + Qdrant doc + no catalog embedding stamp) and `EmbedMissing` (markdown + no Qdrant doc). It does **not** address `downloaded_at`, `conversion`, phantom, or md_path drift — so running `--fix-stamps --reembed` here would be a no-op. The plan's authorization to run it was based on the agent's report that overstated its scope; skipped accordingly.

## Open question

**What sent SIGTERM to Qdrant at 2026-05-01 12:26 UTC?** Journals have rotated; can't pin from logs. Distill stayed up so it wasn't a host reboot. There's no `podman-compose@home-still.service` systemd unit on this host (label artifact only), and the compose file's `restart: unless-stopped` policy correctly does NOT restart on operator stop — but we don't know who issued the stop. If unattributable, this is a real reliability gap: a single-container, single-host Qdrant with no autorestart safety net is the entire embed surface. Tracked in `BACKLOG.md` Operational TODO #3.

## Files changed in this session

- `BACKLOG.md`: P1-15 motivation + acceptance refreshed for 2026-05-02 counts; new P1-19 ("scribe counters advance without artifacts"); Operational TODO entries refreshed (F7 backfill counts) and added (Qdrant outage post-mortem).
- `slop/2026-05-02-distill-qdrant-recovery.md`: this file.
- No code changes. No catalog mutations. The recovery action was a single `podman start`.

## Next steps (not done in this session)

1. Investigate **P1-19** — the stamp-write break. Read `crates/hs-scribe/src/event_watch.rs` (the convert-completion handler) and verify what `total_conversions` actually counts. Check whether the rc.311 NotFound-as-Permanent path is treating recoverable cases as terminal failures that quietly skip the markdown write.
2. Build `hs catalog repair --apply` and `hs catalog purge` (P0-6, P1-15). Without these, the 27 phantoms and 3367 missing `downloaded_at` rows can't be remediated.
3. Lay an autorestart safety net under Qdrant (real systemd unit or `restart: always` with operator override mechanism). Single container, sole embed surface, no monitor — that combination just bit us.
4. Once P1-19 is fixed and drift drains, re-run the original self-test from the top — Phases 2–6 never executed today.
