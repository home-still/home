# Handoff — home-still improvement loop, 2026-10-23

Iteration `2026-10-23-03` (rc.387) is complete: released (2026-10-23
19:05 UTC, workflow green, artifacts verified) and logged. Repo:
`home-still`, branch `main`. The loop prompt is the project command
`/build-loop` (`.omp/commands/build-loop.md`).

## 1. Where things stand

| | state |
|---|---|
| Latest release | `v0.0.1-rc.387` (33 assets; `hs`/`hs-mcp` checksums, arch, marker and `--version` verified) |
| Last commit on `main` | `c8332b5` (rc.387 change), pushed |
| Fleet | big, bmb, two all on **rc.382** — rc.384–387 not deployed (rc.383 withdrawn) |
| Uncommitted | nothing (this file and the rc.387 log entry are committed) |
| Conversion | effectively stopped: 0 stored in ~24 h (see §4) |

## 2. The last iteration: rc.387 — catch-up never re-queues a failed conversion

**Problem.** `hs pipeline catch-up` republished every source with no
markdown, including documents already stamped `conversion_failed`. One run
on 2026-10-21 put 1,086 events on the FIFO `PAPERS` queue: 1,010 retryable
failures, 60 bad sources, 13 already converted, **3** real work. 101 new
downloads queued behind them for two days. `hs status`'s Stalled row (888)
counted stamped failures and told the operator to run catch-up.

**Change (commit `c8332b5`).**
- `hs-common/src/status.rs`: `count_unconverted_stems(.., stamped)` returns
  `UnconvertedStems { all, unstamped }`. `all` → `pipeline_drift`
  (unchanged meaning); `unstamped` → Stalled. Docs on
  `stalled_conversions` / `record_queue` updated (Stalled undercounts while
  old failure events drain — stated, not hidden).
- `crates/hs-mcp/src/main.rs` (`system_status`): builds the stamped set from
  the catalog listing it already has; no catalog ⇒ Stalled unknown.
- `crates/hs/src/pipeline_cmd.rs`: `plan_catch_up(storage, stamped)` skips
  stamped stems and returns them as `skipped_failed`; `cmd_catch_up` reads
  the catalog (fails loudly if it can't), reports
  "Skipped: N failed conversions (R retryable, B not) — re-queue them with
  `hs pipeline reconvert-failed --dry-run`".
- `crates/hs/README.md`: catch-up line. `BACKLOG.md`: new **P1-31** (fixed
  in rc.387) and a note under **P0-28** about the 1,070 queued failure
  events.

**Verified.**
- Live dry runs, same store, back to back
  (`runs/2026-10-23-03/evidence/`): catch-up 2,152 → **144** republished,
  2,008 skipped (1,888 retryable, 120 not). Stalled 888 → **0**; drift
  2,144 → 2,145. The queue was read message-by-message: exactly 144
  unstamped unconverted stems are already queued, so 0 is true.
- fmt clean; clippy clean ×3 (workspace minus hs-scribe; hs-scribe
  default; hs-scribe `server,cuda,eval`, all `--all-targets`).
- Tests (`unshare -rn`, loopback up): workspace 1,581 passed / 8 ignored;
  hs-scribe `--features server` 378; hs-common `--all-features` 374;
  0 failed. rustdoc 30 warnings (= baseline). RECALL@10 25/25.
- Version bake on HEAD: `hs 0.0.1-rc.387`, marker present.

**Released and logged.** Nothing left in this iteration; the next one
starts from §5.

## 3. This session's releases

| rc | change | commit | evidence |
|---|---|---|---|
| 384 | olmocr conversions keep their pages | `7838894`, `d4dc178` | `runs/2026-10-22-06/` |
| 385 | streaming repetition detector fires only on consecutive loops (false positives 96 % → 3.6 % olmocr, 72 % → 0 % glm_ocr; two production-dropped PDFs now convert) | `621afa6` | `runs/2026-10-23-01/` |
| 386 | failed conversions reported by what a retry can do (Failed row, progress 100 % → 88.9 %); `reconvert-failed` re-queues only retryable stamps, `--reason` selects | `e1e52ea` | `runs/2026-10-23-02/` |
| 387 | catch-up skips failed conversions (2,152 → 144 republished); Stalled counts only unstamped (888 → 0) | `c8332b5` | `runs/2026-10-23-03/` |

Also: `/build-loop` project command (`c9f8f99`).

## 4. Operator actions waiting (none can be done by the loop)

Full write-up: `docs/loop/needs-decision/gapped-drops-and-dead-olmocr.md`.

1. **Upgrade bmb** (`hs upgrade --pre --yes` there). Until it runs rc.385,
   every glm_ocr conversion with ordinary repeated phrasing is refused as
   gapped and terminated — 87/87 terminations in a 6 h window were this.
2. **Stop big's idle scribe:** `sudo systemctl stop hs-serve-scribe`
   (`:7433`, disabled, not in the pool, 0 conversions, running since an
   accidental start 2026-10-02, holds 2,340 MiB). Without it ~16.4 GB is
   free, above vLLM's ~15.7 GB, so the olmocr tier can start (P0-27).
3. **After 1:** recover the dropped documents —
   `hs pipeline reconvert-failed --dry-run`, then staged
   (`--reason gapped_conversion --limit 25`). Note ~1,010 retryable events
   are *already* queued from the 2026-10-21 catch-up; they will retry on
   their own once bmb is upgraded, so re-queue only what is still stamped
   after the queue drains.
4. Upgrade big and two too (rc.386 status/MCP changes need CLI and MCP on
   the same version; an older MCP leaves the Failed row blank).
5. **P0-26:** `ufw` rule for the docs port (7446) on big — remote document
   links stay dead until then.

## 5. Top remaining loop items

1. Operator actions in §4 (nothing else moves the conversion backlog).
2. **P0-27:** derive the olmocr VRAM gate from vLLM's real requirement
   (`--gpu-memory-utilization` × card memory, in `~/.home-still/run-olmocr.sh`)
   instead of `HS_SCRIBE_VRAM_HEADROOM_MB=13500`.
3. **P1-27:** olmocr cold starts fail whole documents; wait for llama-swap
   to report the model `ready` before invoking olmocr.
4. **P1-29:** 24/200 sampled accepted documents contain verbatim loops
   (mostly scribe-vlm); enumerate offline with the rc.385 rule.
5. **P1-30:** bmb's `com.home-still.scribe-autotune` crash-loops
   (`unrecognized subcommand 'autotune'`); `/tmp/hs-scribe.log` is 1.1 GB.

## 6. Working notes and traps

- **Builds:** `CARGO_TARGET_DIR=~/hs-target-11`, toolchain
  `+1.99.0`. Release-profile builds of any crate (examples included) fail
  in `build.rs` unless `HS_RELEASE_TAG` is set.
- **Tests under `unshare -rn`:** bring loopback up
  (`sh -c 'ip link set lo up && …'`) or ~72 local-server tests fail with
  `os error 101`.
- **Background jobs** are killed when the session is interrupted; re-check
  logs before trusting a "running" job.
- **Smoke-testing status/MCP without touching live services:** run the new
  `hs-mcp --serve 127.0.0.1:7495` and point the CLI at it with
  `HS_MCP_URL=http://127.0.0.1:7495/mcp`. Live MCP is `:7445`.
- **Catalog analysis:** S3 credentials are in `~/.home-still/secrets.env`
  (`HS_S3_ACCESS_KEY` / `HS_S3_SECRET_KEY`; config.yaml holds `${…}`
  placeholders). A read-only copy syncs to `/tmp/catalog_copy` with
  `aws s3 sync` in ~30 s. Never print the secrets.
- **Queue inspection:** NATS monitoring at `http://<nats-host>:8222/jsz?streams=true&consumers=true`;
  a `nats` CLI is at `/tmp/nats` (fetched release binary).
  `nats stream get PAPERS <seq>` reads a work-queue message without
  consuming it. Journal lines carry ANSI codes — strip them before
  regexing `key=`.
- **bmb glm backend for end-to-end tests:** llama-swap on bmb needs a
  bearer key (in the LaunchAgent plist, `HS_SCRIBE_OPENAI_API_KEY`); bmb
  answers 429 when production holds its slots — use
  `HS_SCRIBE_VLM_CONCURRENCY=2`. Run `hs-scribe-server` directly (`hs serve
  scribe` refuses while a systemd scribe unit exists).
- **Committed docs:** no real IPs, hostnames or keys — use `<host>`,
  `<glm-host>`. Converted paper text is not committed (digests only).
- **Do not** run `hs upgrade`, restart live units, publish events, or purge
  anything from the loop; dry runs only.
