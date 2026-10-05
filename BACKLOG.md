# home-still — Backlog

Prioritized stories from the 10-agent codebase audit. Ordered highest priority → lowest.
Security and documentation stories are intentionally excluded.

## Working rules for every story in this file

- **Greenfield — no backwards compatibility.** When a story says "delete", it means delete the code, not wrap it in a feature flag, not keep a legacy alias, not leave a deprecation stub. Callers get fixed in the same PR.
- **ONE PATH per feature.** No fallbacks, legacy shims, stub placeholders, "backup" modes, rollover behavior, compatibility branches. When the primary path can't produce a usable result, fail loudly. If a fix needs a "fallback" to work, the fix is wrong — redesign.
- **No CPU fallback.** Anywhere the audit says "tie to compute_device" or "fail if CUDA missing", the answer is fail — not a CPU path.
- **Definition of Done:** exactly the gates `.github/workflows/ci.yaml` runs (and `release.yaml` re-runs on the tagged commit before it builds anything), each with `--locked` as CI has it: `cargo fmt --all --check`; `cargo clippy --workspace --exclude hs-scribe --all-targets -- -D warnings`; `cargo test --workspace --exclude hs-scribe`; `cargo clippy -p hs-scribe -- -D warnings`; and the `--features` invocations the workspace forms do not compile — `cargo clippy -p hs-scribe --features server --all-targets -- -D warnings`, `cargo test -p hs-scribe --features server`, `cargo clippy -p hs-distill --features cuda --all-targets -- -D warnings` (the exact feature set `hs-distill-server` ships with; compiles without a GPU) and `cargo test -p hs-distill --features server --lib` (the `server`-gated `qdrant` / `server` / `embed::onnx` modules). All pass before tagging any `rc.*`. The plain `--workspace` forms cover none of the `--features server` code and nothing platform-specific; running only those is how `Check` stayed red from 2026-09-08 to 2026-09-19 (8 consecutive failures) while releases shipped green. Release builds (`--release`) additionally require `HS_RELEASE_TAG=vMAJOR.MINOR.PATCH[-PRE]`; see `build-support/version.rs`.

---

## P0 — Non-negotiable violations (must fix before next rc.*)

### P0-16. `big_mac` cannot run any `hs` command — startup panics on the log spool dir (2026-09-08)
**Motivation:** During the rc.352 fleet upgrade, `big_mac` could not be upgraded and remains stranded on **rc.326** (the rest of the fleet is on rc.352). Every `hs` invocation — including `hs --version` and `hs upgrade` — aborts before doing any work:
```
thread 'main' panicked at crates/hs/src/main.rs:75:37:
install logging subscriber: opening spool dir "/Volumes/home-still/logs/spool/hs"
Caused by: Permission denied (os error 13)
```
Two separate defects: (a) `/Volumes/home-still` on `big_mac` is not writable by the invoking user, so the configured `log_dir` is wrong for that host or the mount lost its permissions; (b) `hs` treats an unwritable log directory as a fatal panic at startup, which makes the binary unusable for *every* command — including the `upgrade` that would replace it. A host that cannot log should still be able to report its version and upgrade itself. This is also why `big_mac` cannot self-recover: the fix cannot be delivered by `hs upgrade` because `hs upgrade` is the thing that panics.
**Scope:**
- `crates/hs/src/main.rs:75` — the `.expect()` / panic on logging-subscriber install
- `hs_common::logging` spool-dir creation
- `big_mac`'s `~/.home-still/config.yaml` `home.log_dir` / `/Volumes/home-still` mount permissions
**Change:** Startup must not panic because logging could not be initialised. Failing to open the spool dir is not a failure of the requested command — emit one diagnostic to stderr naming the unwritable path and continue with console-only logging. Keep it loud (a warning on every invocation, not a silent degrade) but non-fatal. Separately, correct `big_mac`'s `log_dir` so the spool lands on a writable path. Do NOT add a silent fallback that hides the misconfiguration.
**Acceptance:**
- With `/Volumes/home-still` unwritable, `hs --version` and `hs upgrade --pre` both succeed on `big_mac` and print a warning naming the path.
- `big_mac` reports `hs 0.0.1-rc.352` or later after a self-upgrade.
- No code path silently swallows the spool-dir error without surfacing it.

**2026-09-16 — second host, same defect.** `two` (the cloud gateway) was bricked identically and sat on **rc.345** through the rc.352 rollout. Its `~/.home-still/config.yaml` was overwritten on 2026-09-12 08:36 with a byte-identical copy of `big`'s (same md5), which carries `home.project_dir: /mnt/home-still` — a path that exists on `big`, does not exist on `two`, and cannot be created under root-owned `/mnt`. Every `hs` invocation aborted in `hs_common::logging::init` → `Spool::new` (`hs-common/src/logging/mod.rs:52`), `hs --version` and `hs upgrade` included, so the host could not self-recover. Restored on 2026-09-16 by rewriting the host's own config (`project_dir: ~/home-still`) and upgrading to rc.352; the code defect stands. This is not a `big_mac` / `/Volumes` quirk: any config drift that points `log_dir` at an unwritable path takes the entire CLI down on any host, and the blast radius is every command rather than logging.

**FIXED rc.353 (2026-09-16).** `hs_common::logging::init` no longer panics on an unopenable spool dir: it prints one `eprintln!` naming the service and the path (`hs-common/src/logging/mod.rs:55-70`), keeps a `spool: None` handle, and continues stderr-only — covered by the `spool dir that cannot be opened must degrade` test in that module. `big_mac` and `two` both self-upgraded and report `hs 0.0.1-rc.355`.

### P0-17. `hs upgrade` claims success for services it failed to restart, and skips others entirely (2026-09-08)
**Motivation:** Two gaps observed during the rc.352 rollout, both of which leave upgraded binaries running old code with no signal to the operator.

1. On `big`, `hs upgrade` restarted only `hs-serve-distill`, `hs-serve-mcp` and the distill containers. It installed a new `hs-scribe-server` binary but never restarted `hs-serve-scribe-olmocr`, and it does not touch the `systemd --user` daemons `hs-scribe-watch-events` / `hs-distill-watch-events` — which are exactly where the rc.352 conversion-gate and page-count fixes live. Without a manual `systemctl restart`, `hs upgrade` reports "Upgraded to 0.0.1-rc.352" while the changed code is not running.
2. On `mac_air`, the restart of `com.home-still.scribe` printed `Unload failed: 5: Input/output error` and `Load failed: 5: Input/output error`, then printed `OK: com.home-still.scribe restarted` and counted it in `Restarted 1 service(s)`. A failed `launchctl` round-trip must not be reported as OK.
**Scope:**
- `hs upgrade` service-restart logic in `crates/hs` (the restart table and its launchctl/systemd branches)
- the restart set: must include `hs-serve-scribe-olmocr` and the `--user` scope watcher units
**Change:** Derive the restart set from which binaries were actually replaced, covering both `systemd --system` and `systemd --user` scopes plus launchd. Propagate each restart's real exit status: a non-zero `launchctl`/`systemctl` result is a failure, must be printed as such, must not increment the restarted count, and must make `hs upgrade` exit non-zero. Per CLAUDE.md, fix `hs upgrade` rather than documenting a manual `systemctl restart` step.
**Acceptance:**
- After `hs upgrade` on `big`, every process whose binary changed reports the new version with no manual restart.
- A forced `launchctl` failure on a Mac host makes `hs upgrade` print the failure and exit non-zero.
- `hs upgrade` never prints `OK: <svc> restarted` for a restart whose underlying command failed.

**2026-09-16 — third observation, `two`.** `hs -y upgrade --pre --force` replaced `hs`, `hs-gateway` and `hs-mcp`, then printed `No running services found to restart` while `hs-gateway.service` was `active`. `crates/hs/src/restart_cmd.rs:16` iterates a hardcoded `["scribe", "distill", "mcp"]` and builds unit names as `hs-serve-{type}`; `two`'s units are `hs-gateway.service` and `hs-mcp.service`, so neither is ever considered. The gateway went on executing the deleted rc.345 image until a manual `systemctl restart hs-gateway`. `hs-gateway` is missing from the restart set on *every* host, not just `two` — deriving the set from the binaries actually replaced (as this story already requires) fixes both halves.

**FIXED rc.353–355 (2026-09-16).** The hardcoded `["scribe", "distill", "mcp"]` table is gone. `crates/hs/src/restart_cmd.rs` discovers units from `systemctl list-units` (system **and** `--user`) plus `launchctl list`, resolves each unit's `ExecStart` / `ProgramArguments` binary — through the kernel's `" (deleted)"` marker — and restarts exactly those units whose executable was replaced (`select_units` / `matches_replaced`). Every `systemctl`/`launchctl` non-zero exit becomes a named failure string, is excluded from the restarted count, and makes the command exit non-zero; restarts are re-verified with `systemctl show` / `launchctl print` before being reported OK. Verified post-rc.355 on `big` (`hs-serve-mcp`, `hs-serve-scribe-olmocr`), `two` (`hs-gateway.service`, which the old table could never name) and `bmb` (`com.home-still.scribe`).

### P0-18. `hs-gateway` silently falls back to loopback + zero routes when `cloud.gateway` is missing (2026-09-16)
**Motivation:** `GatewayConfig::load` (`crates/hs-gateway/src/config.rs:85-91`) returns `Self::default()` when the `cloud.gateway` section is absent, which means `listen: 127.0.0.1:7440` and an **empty** `routes` map. On `two` the whole `cloud:` section was gone from 2026-09-12 to 2026-09-16 (see P0-16); the public gateway kept working only because its process had started on 2026-06-23 and still held the old config in memory. Any restart in that window — a reboot, a `Restart=on-failure` bounce, or the rc.352 upgrade — would have bound loopback only and dropped every `https://cloud.lolzlab.com` client, while `/health` kept answering `ok` to a local probe. A missing gateway config is a misconfiguration, not a set of defaults, and "listen somewhere else than configured" is the most expensive default in the fleet.
**Scope:**
- `crates/hs-gateway/src/config.rs:75-92` — `GatewayConfig::load` and the `Default` impl it leans on
- `crates/hs-gateway/src/main.rs` — startup path that consumes the loaded config
**Change:** `load()` must fail loudly when `cloud.gateway` is absent, naming the config path and the missing key, and the process must exit non-zero instead of starting a gateway that routes nothing. Per-field serde defaults for genuinely optional knobs (`token_ttl_secs`, `refresh_ttl_secs`, `key_rotation_days`, `secret_path`) stay; `listen` and `routes` must come from the file. Do not add a "degraded gateway" mode.
**Acceptance:**
- With the `cloud:` section removed, `hs-gateway` exits non-zero and prints the config path plus `cloud.gateway`; it never binds a port.
- With the section present, startup is unchanged (`Starting gateway on 0.0.0.0:7440`, `Routes: [...]`).
- No code path substitutes `127.0.0.1` for a configured listen address, and no path starts the gateway with an empty route map.

**FIXED rc.353 (2026-09-16).** `GatewayConfig::from_yaml` — split out of `load()` so the rule is testable without `$HOME` — errors with the config path plus a "missing `cloud.gateway` section" message (`crates/hs-gateway/src/config.rs:86`) and bails on an empty `routes` map (`:92-97`). No `Self::default()` path remains. `two`'s gateway restarted on rc.355 binding its configured listen address with a populated route map.

### P0-15. `scribe_health` reports `ok` without ever probing its VLM backend (2026-07-29)
**Motivation:** `big`'s olmocr backend was dead from **2026-07-25T22:31Z to 2026-07-29T12:17Z** — llama-swap could not start vLLM (`--gpu-memory-utilization 0.70` needed 16.49 GiB against 16.06 GiB free once the distill embedder was pinned resident). For those four days `curl :7435/health` returned `{"status":"ok","layout_model":true,"table_model":true,...}` and `hs status` listed the instance as `healthy: true, slots_available: 12`, because the health handler only checks scribe's own in-process layout/table models. It never issues a request to `HS_SCRIBE_OLMOCR_ENDPOINT`. The scribe pool therefore kept dispatching to a backend that could only time out, and the sole outward signal was `pipeline_drift` climbing to 468 against a threshold of 3 — a lagging indicator nobody is paged on. A green health check in front of a dead dependency is a silent-failure path.
**Scope:**
- scribe server health handler in `crates/hs-scribe` (the `/health` route backing `scribe_health`)
- `crates/hs-mcp/src/main.rs` — `scribe_health` tool response shape
- pool readiness filter in `hs-scribe-watch-events` (the check that already readiness-excludes a sleeping `bmb`)
**Change:** `/health` must probe the configured VLM backend and report its real state — one cheap upstream call (`GET {endpoint}/models`, or a cached last-success timestamp with a max age). Report the backend verdict as its own field (`backend_reachable`, `backend_model`, `backend_checked_at`) and make the top-level `status` fail when the backend is unreachable. `status: "ok"` must mean "this instance can convert a PDF right now", not "my own models loaded". The pool's readiness filter then excludes a backend-dead instance the same way it excludes a sleeping laptop. No degraded/partial status tier — reachable or fail.
**Acceptance:**
- With llama-swap stopped on `big`, `curl :7435/health` returns a non-ok status and `hs status` shows that instance unhealthy within one poll interval.
- The scribe pool dispatches zero conversions to an instance whose backend probe is failing.
- `last_conversion_at` going stale while papers are queued is surfaced, not silent.

**FIXED rc.356 (2026-09-21) — first two acceptance bullets; third re-scoped below.** `/health` now runs an admission probe before answering (`crates/hs-scribe/src/backend_probe.rs`): `GET {swap_base}/running` on the configured olmocr endpoint decides `model_resident`, and when the model is not resident the verdict falls back to `hs_common::gpu::free_vram_mb() >= vram_headroom_mb` (default 15000, `HS_SCRIBE_VRAM_HEADROOM_MB`). Verdict cached 5 s; logged once per *transition* with the holder list, not once per probe. `status` is `"ok"` only when the backend can take work; otherwise HTTP 503 + `status: "backend_unavailable"` plus `backend_reachable` / `backend_model_resident` / `backend_vram_free_mb` / `backend_checked_at`. `/readiness` reports `vlm_slots_available: 0` and `backend_status: "backend_unavailable"`, which is the ineligibility signal `ServicePool::try_pick_once` already honours — no new dispatch policy site. The gate is scoped to `converter == olmocr`: legacy/Ollama instances (bmb) have no `/running` endpoint and are not gated on a probe of something they never talk to.

Measured on `big` after the rc.356 upgrade, card contended at 4938 MiB free: `curl :7435/health` → `503 {"status":"backend_unavailable","backend_reachable":true,"backend_model_resident":false,"backend_vram_free_mb":4938}`; `/readiness` → `{"ready":false,"vlm_slots_available":0,"backend_status":"backend_unavailable"}`; MCP `system_status` → `{"url":"http://192.168.1.110:7435","healthy":false,"activity":"backend unavailable"}`; `hs scribe convert --server http://localhost:7435` exits 1 with the free-VRAM message instead of hanging. `hs-scribe-watch-events` took 10 redelivered `papers.ingested` events and dispatched **zero** of them (`in_flight_conversions: 0`, no `dispatching` line) — before rc.356 each would have been sent into a backend that could only burn llama-swap's `healthCheckTimeout`. Exactly one `vlm backend unavailable — refusing dispatch` line per verdict transition, carrying `holders=pid=… llama-server; …`.

**Still open (moved out of P0-15):** the third bullet — alarming on a stale `last_conversion_at` while papers are queued — is not implemented. The field is reported, not alarmed. The silent-staleness *cause* this story was filed for (a dead backend reporting `ok`) is closed; a general liveness alarm is a separate observability item.

### P0-12. Stop VLM repetition collapse from being committed (F1, rc.308 self-test)
**Motivation:** Self-test rc.308 round-trip on `10.48550_arxiv.2312.10997` (Gao RAG survey) produced page-1 markdown that's `"the retrieval of"` repeated for ~9 KB, then `"valval...val"` for the remainder. Convert stamped `success`, auto-embed indexed 35 chunks; the doc now poisons `academic_papers` Qdrant collection. `event_watch.rs:171-176` documents that the QC repetition-loop reject was disabled by operator decision on 2026-04-23 because rejection produced an infinite retry storm. The retry-storm root cause: rejection didn't close out the catalog row, so the inbox watcher re-detected the source PDF and re-queued it. Same input → same VLM output → same rejection → loop. The current "save what we can" path is a ONE-PATH violation.
**Scope:**
- `crates/hs-scribe/src/event_watch.rs:167-177` (PDF/VLM branch)
- VLM serving call site within `crates/hs-scribe` (decode-param config)
- `crates/hs-scribe/src/postprocess.rs` (`clean_repetitions`)
- Inbox watcher source-scan in `crates/hs/src/scribe_inbox.rs` (or equivalent — the loop that re-queues sources without `conversion.completed_at`)
- New: `crates/hs/src/scribe_cmd.rs` — add `hs scribe reconvert <stem>`
**Change (ordered, all four required — ONE PATH):**
1. **Decode-time prevention is primary.** Tighten the VLM call: set `repetition_penalty`, `no_repeat_ngram_size`, and per-page stop conditions; verify `max_new_tokens` is not silently truncating mid-loop. The 9.4 KB outlier on page 1 (5× the median for that paper) means the model rode the loop to `max_new_tokens` before stopping — generation params are under-constrained. Run the F1 round-trip as a regression check.
2. **Restore the QC gate as a backstop that terminally fails the catalog row.** Add a longest-contiguous-repeated-substring length check on raw VLM output before `clean_repetitions` runs. On trip, do NOT call `clean_repetitions`; instead stamp `catalog.conversion = { completed_at: now, success: false, reason: "vlm_repetition_loop", server: "scribe-vlm" }` via `update_conversion_catalog_via`, then return `HandlerError::Permanent`. The rejection path writes the same catalog fields the success path writes — only `success` differs. This is what broke the previous attempt at rejection: the rejection path didn't write `completed_at`, so the source kept re-queueing.
3. **Inbox watcher must respect terminal failure.** Confirm the source-scan loop skips stems whose catalog row has `conversion.completed_at` set, regardless of `success`. If it currently filters on `success`, change it to filter on `completed_at`. Document the invariant: "a source PDF with a completed conversion row is never re-queued; operators use `hs scribe reconvert` to retry."
4. **Add `hs scribe reconvert <stem>`** that clears `conversion.completed_at` and re-publishes the convert event. CLI-only (per memory `feedback_destructive_ops_cli_only`). Without this, terminal failures are stuck.
**Acceptance:**
- `paper_download` then `scribe_convert` on `10.48550/arxiv.2312.10997` either produces a clean page-1 OR stamps `conversion.success: false, reason: "vlm_repetition_loop"` AND does not re-queue.
- `cargo test` includes a unit test for the longest-run check with a synthesized "the retrieval of" × N input.
- Manually deleting `conversion` from a catalog row + a single `hs scribe reconvert <stem>` re-runs the convert exactly once.

### P0-13. Reject structurally-empty markdown at indexing time (F3, rc.308 self-test)
**Motivation:** `0003122412445802 -- ... -- Anna's Archive` is 7482 B / 29 pages of `<table>...empty cells...</table>` skeletons + `---` page separators. Catalog says `embedded:true`, chunks are in Qdrant. The `embedding_skipped: zero_chunks_or_empty` gate caught the pure-`---` case (`00224499_2013_838934`, 112 B) but not this one because trimmed tag-soup is not empty. `crates/hs-distill/src/pipeline.rs:142-145` returns `Ok(0)` silently when chunks are empty; no skip-reason stamp.
**Scope:**
- `crates/hs-distill/src/quality.rs` (`is_low_quality`)
- `crates/hs-distill/src/pipeline.rs:51-54` and `:142-145`
- `crates/hs-distill/src/event_watch.rs` — translate the new error into the existing `embedding_skip: zero_chunks_or_empty` stamp
**Change:**
1. `is_low_quality`: strip HTML tags before measuring non-whitespace character density. A chunk that is 100% tag content has zero semantic signal.
2. Replace both silent `Ok(0)` returns with `Err(DistillError::EmptyAfterFilter)`. Update `event_watch.rs` to map this to `record_embedding_outcome_via(..., embedding_skip: "zero_chunks_or_empty", ...)` + `HandlerError::Permanent`. Catalog must distinguish "indexed 0 chunks" (a success path that should not exist) from "rejected" (skip stamp).
**Acceptance:**
- A synthetic markdown of empty `<table>` cells + `---` separators produces `embedded:false, embedding_skipped:true, embedding_skip_reason:"zero_chunks_or_empty"`.
- No `Ok(0)` codepath remains in `pipeline.rs`; the indexer either embeds at least one chunk or stamps a skip reason.

### P0-14. Add longest-run gate to `distill_scan_repetitions` (F2, rc.308 self-test)
**Motivation:** Scanner counts cleanup *truncation sites* (per-line collapses), not run length. The F1 poisoned doc (one continuous 9.4 KB run = one site) returns `flagged_count == 0` at threshold=5. The 57-doc HTML cluster gets caught only because page-breaks fragment the loops into 168–256 sites each. The "expected 0 in steady state" QC baseline is unreachable through this scanner alone.
**Scope:**
- `crates/hs-mcp/src/main.rs` (`distill_scan_repetitions` handler, ~line 2163)
- `crates/hs-scribe/src/postprocess.rs` if the longest-run helper lives upstream
**Change:** After `clean_repetitions`, additionally compute the longest contiguous repeated-substring run on the **original** markdown. Flag if either the truncation count OR the longest-run length crosses its respective threshold. Surface both signals in the per-doc result. Single threshold semantics — no parallel "high-confidence" channel. With **P0-12** in place, this scanner becomes a corpus-cleanup tool for already-poisoned docs, not a primary gate.
**Acceptance:** A synthetic 10 KB single-loop input produces `flagged: true` with `longest_run_bytes` populated; the existing 57-doc cluster still flags at threshold=5.

### P0-1. Delete all `reqwest::Client::new()` silent fallbacks
**Motivation:** Four sites silently replace a configured client with an unconfigured one on `builder().build()` failure, losing timeouts and proxy settings. Pure ONE-PATH violation.
**Scope:**
- `hs-common/src/service/registry.rs:45`
- `hs-common/src/compose.rs:137`
- `crates/hs/src/upgrade_cmd.rs:446`
- `crates/hs-scribe/src/client.rs:149`
**Change:** Add one helper `http_client(timeout: Duration) -> Result<reqwest::Client>` in `hs-common` that returns the error. Delete every `unwrap_or_else(|_| Client::new())` site and call the helper. No fallback branch, no default client — if builder fails, propagate the error.
**Acceptance:** `rg 'unwrap_or_else.*Client::new\(\)' crates hs-common paper` returns zero matches.

### P0-2. Delete the `local-html` legacy converter path
**Motivation:** `cmd_clean_junk` still filters on `server == "local-html"` — this is the legacy dual-converter path that is supposed to be gone.
**Scope:** `crates/hs/src/scribe_cmd.rs:784`
**Change:** Delete the filter branch and any code that writes `"local-html"` as a server identifier anywhere in the tree. If rows with `server == "local-html"` still exist in the catalog, write a one-shot migration in `hs migrate` that deletes them and fail loudly on encounter from any other caller. No silent skip.
**Acceptance:** `rg 'local-html|local_html' crates hs-common paper` returns zero matches outside of a single migration step.

### P0-3. Delete `discover_or_fallback` in service registry
**Motivation:** Literal fallback API in `hs-common/src/service/registry.rs:94-112`. ONE-PATH violation by name.
**Scope:** `hs-common/src/service/registry.rs`
**Change:** Delete `discover_or_fallback` and every caller's fallback list argument. Discovery either succeeds or errors; no hardcoded defaults, no static fallback pool.
**Acceptance:** Symbol `discover_or_fallback` does not exist. Every caller now uses `discover` and propagates the error.

### P0-4. Delete storage-backend fallback in hs-mcp startup
**Motivation:** `crates/hs-mcp/src/main.rs:270-305` silently falls back to `LocalFsStorage` when storage config is absent or invalid — hides typos until runtime misbehavior.
**Scope:** `crates/hs-mcp/src/main.rs`
**Change:** Delete the fallback branch. If storage config is missing or fails to load, the process exits non-zero with the parse error.
**Acceptance:** Running `hs-mcp` with an empty/broken storage config produces a clear error and exits immediately.

### P0-5. Delete `Url::parse` fallback in OCR Ollama client
**Motivation:** `Url::parse(url).unwrap_or_else(|_| Url::parse("http://localhost:11434").unwrap())` in `crates/hs-scribe/src/ocr/ollama.rs:23` silently rewrites bad URLs to localhost and has a nested `.unwrap()`.
**Scope:** `crates/hs-scribe/src/ocr/ollama.rs`
**Change:** Constructor returns `Result<Self>`; URL parse errors propagate. No default URL, no nested unwrap.
**Acceptance:** Passing a bad URL returns an error at construction time; no panic possible on any call site.

### P0-6. Remove destructive operations from MCP surface
**Motivation:** Destructive-ops-on-MCP violates the user-memory rule "Destructive ops: CLI only." Agents can currently mass-delete via MCP.
**Scope:** `crates/hs-mcp/src/main.rs`
- `distill_purge` tool (lines 2099-2114) — delete the tool registration. Move the operation to `hs distill purge <doc_id>` if it doesn't already exist.
- `catalog_repair` tool (lines 784-850) — split into read-only `catalog_repair_report` (dry-run-only, MCP safe) and CLI-only `hs catalog repair --apply`. Delete the apply path from MCP.
- `dedupe_url_encoded` tool (lines 1402-1450) — same treatment; MCP exposes only the dry-run report; CLI owns `--apply`.
**Change:** No `destructive_hint` tools on MCP. No `apply` mode reachable via MCP at all. CLI subcommands own every write-deletes-data path.
**Symmetrical CLI work:** `hs catalog purge <stem>` is added in **P1-15** to fill the operator gap left by removing destructive `catalog_repair --apply` from MCP. Track jointly with P1-15.
**Acceptance:** Every remaining MCP tool either reads or modifies a single known document. No bulk-delete, no bulk-rebuild, no repair-apply.

### P0-7. Enforce compute_device in distill config
**Motivation:** `crates/hs-distill/src/config.rs:42-43` has no `compute_device` field. Device is auto-detected from nvidia-smi. Your non-negotiable is explicit: `compute_device: cuda` stays in config — detection is not enforcement.
**Scope:** `crates/hs-distill/src/config.rs`, `crates/hs-distill/src/embed/onnx.rs`
**Change:** Add `compute_device: ComputeDevice` to `EmbeddingConfig` (no `Option` — required field, no default). Remove auto-detection. `OnnxEmbedder::new` reads the config field; if `cuda` and probe fails, return an error. There is no CPU variant of `ComputeDevice` that ships — delete it if present. If a user needs CUDA broken locally, they fix CUDA, not the config.
**Acceptance:** Config without `compute_device` fails to load with a clear error. There is no code path that instantiates a non-CUDA embedder in the distill binary.

### P0-8. Mandate `--features cuda` in distill build guidance and runtime
**Motivation:** `crates/hs/src/distill_cmd.rs:265` only mentions `--features server`. Silent CPU fallback is caught only by the VRAM probe.
**Scope:** `crates/hs/src/distill_cmd.rs`
**Change:** Build-instruction error text and any `cargo` invocation the code generates must include `--features cuda`. Add a runtime assertion at distill-server startup that the binary was compiled with the `cuda` feature (`#[cfg(feature = "cuda")]`); if not, exit non-zero immediately.
**Acceptance:** A non-CUDA build refuses to start. Error messages point to the exact cargo command.

### P0-9. Fix catalog write integrity
**Motivation:** `hs-common/src/catalog.rs:175, 178` discards `fs::write` and `create_dir_all` errors via `let _ =`. Catalog entries silently vanish on disk-full, permission errors, or parent-missing.
**Scope:** `hs-common/src/catalog.rs`
**Change:** `write_catalog_entry` returns `Result<()>`. Propagate through every `update_*_catalog` function (193, 221, 454, 483, 540, 551, 573). Every caller handles the error; no `.ok()` discards.
**Acceptance:** `rg 'let _ = .*write|fs::write.*\.ok\(\)' hs-common/src/catalog.rs` returns zero matches.

### P0-10. Fix catalog read error fidelity
**Motivation:** `hs-common/src/catalog.rs:259` — `read_catalog_entry_via` chains `.ok()` on both the storage GET and the YAML parse, collapsing transient S3 errors and corrupt rows into "not found." Orphan-detection logic can't distinguish the two.
**Scope:** `hs-common/src/catalog.rs`
**Change:** Return `Result<Option<Entry>>`. Storage errors → `Err`. Missing object → `Ok(None)`. Parse errors → `Err` (a corrupt row is not an orphan).
**Acceptance:** Every caller handles all three states explicitly.

### P0-11. Fix inbox dedup duplicate-drops
**Motivation:** `hs-common/src/inbox.rs:85` treats any storage error as "not found" via `.unwrap_or(false)`, causing duplicate drops on transient S3 faults. Line 110 publishes an empty NATS payload via `to_vec().unwrap_or_default()` on serde failure.
**Scope:** `hs-common/src/inbox.rs`
**Change:** Replace `.unwrap_or(false)` with explicit error propagation — transient errors bubble up and the caller retries or fails. Replace `to_vec().unwrap_or_default()` with `?` — a serde failure must fail the publish, not send garbage.
**Acceptance:** No `unwrap_or(false)` or `unwrap_or_default()` on storage/serde results anywhere in `inbox.rs`.

---

## P0 — Blockers

### P0-1. `big_mac` cannot run `hs` at all — panics on logging init, stuck at rc.326
**Symptom:** every `hs` subcommand that initializes logging panics immediately:

```
thread 'main' panicked at crates/hs/src/main.rs:75:37:
install logging subscriber: opening spool dir "/Volumes/home-still/logs/spool/hs"
Caused by: Permission denied (os error 13)
```

`hs --version` still works (it short-circuits before logging init), which is why
the host reports a version and looks healthy. **`hs upgrade` does not** — so
`big_mac` has been unable to self-upgrade and sits at **rc.326** while the fleet is
at rc.350 (discovered 2026-08-10 during the rc.350 deploy; skew predates it).
**Scope:** `/Volumes/home-still` mount permissions on `big_mac`, and
`crates/hs/src/main.rs:75`.
**Change:** Fix the mount/ownership so the spool dir is writable. Separately,
consider whether an unwritable *log* directory should be fatal to every command —
this is a logging concern taking down the entire CLI, including the one command
(`upgrade`) that could repair the host.
**Acceptance:** `ssh big_mac '~/.local/bin/hs upgrade --pre -y'` completes and
`hs --version` reports the current rc.

---

## P1 — Reliability (panics and silent failures in hot paths)

### P1-0. distill server embeds 0 chunks for some glm_ocr docs whose markdown chunks fine locally
**ROOT CAUSE FOUND + FIXED 2026-08-10 (awaiting rc.350 deploy).** Not glm_ocr-specific
and not a chunker divergence. `pipeline.rs::index_document` gated on
`hs_common::html::is_paywall_html`, an *HTML* heuristic, applied to *converted
markdown*. Its first rule — `has_login && content.len() < 100_000` — carries no
`!has_article` guard, so any sub-100 KB document containing "sign in" / "log in" /
"access denied" was rejected outright; a second rule rejected anything mentioning
"clinical trials" / "search results" without a literal `abstract`+`references` pair.
`hs distill diagnose` never runs this gate, which is exactly why CLI and server
disagreed. Measured over all 423 `zero_chunks_or_empty` rows: **278 rejected by the
gate, of which `is_known_interstitial` (the in-tree false-positive-safe detector)
clears 277** — including the full text of *Accelerate* (399 KB) and a 99 KB
mathematics-education paper. Fix: gate on `is_known_interstitial` instead; all
literal interstitial signatures still match, and `hs-scribe` keeps `is_paywall_html`
on raw HTML where it belongs. Regression tests in `hs-common/src/html.rs`. A further
132 docs carried stale pre-rc.349 stamps and were recovered by
`distill_backfill(retry_skipped=true)` with no code change. Full analysis:
`docs/research/2026-08-10-home-still-repair-report.md`.
**Follow-up still open:** the silent `Ok(0)` remains — a gate veto is
indistinguishable from "chunker produced nothing". Give it a distinct
`embedding_skip.reason` so it fails loudly.

**Motivation:** Discovered 2026-07-05 ingesting Game AI Pro 2. Three chapters
(`GameAIPro2_Chapter11_Smart_Zones...`, `..._Chapter39_Analytics-Based_AI...`,
`..._Chapter40_Procedural_Content_Generation...`) are stamped
`embedding_skip: zero_chunks_or_empty` despite having complete, high-quality
markdown in storage (Ch39: 61 658 B / 9 051 words, `converted_by: glm_ocr`,
`markdown_path` correct). `hs distill diagnose <stem>` reads that same storage
markdown and reports **20/18/9 chunks, all accepted, 0 rejected** — but the
distill **server** path (`distill_index` / `distill_reindex` MCP, and the
original event-driven embed) returns `chunks_indexed: 0, old_vectors_purged: 0`
and re-stamps `zero_chunks_or_empty`. A known-good control (`..._Chapter08...`,
olmocr) reindexes correctly (purged 10, indexed 10) against the *same* server,
so the server/embedder is healthy and the bug is **doc-specific**. Common factor
of the failing set: `converted_by: glm_ocr`. So the CLI-side chunker (diagnose)
and the server-side chunk/embed path diverge on specific glm_ocr markdown — one
sees 20 chunks, the other sees 0. This silently drops real documents from search
and likely accounts for a slice of the corpus-wide `embedding_skipped: 466`.
**Scope:**
- `crates/hs-distill/src/pipeline.rs` (server read→chunk→embed path)
- `crates/hs-distill/src/event_watch.rs` (`zero_chunks_or_empty` stamp site)
- Whatever markdown-read the server uses on reindex vs. what `hs distill diagnose`
  uses in `crates/hs/src/distill_cmd.rs::cmd_diagnose` — reconcile the two so they
  read + chunk identically. Suspect: server reconstructs from catalog `pages`
  offsets or applies a different pre-chunk normalization than diagnose's raw read.
**Change:** Make the server reindex path produce exactly what `diagnose` produces
for the same stem (single source of truth for markdown-read + chunk). Fail loudly
if a doc that diagnose says yields N>0 chunks embeds 0 — do not silently stamp
`zero_chunks_or_empty` when the chunker would accept chunks.
**Acceptance:**
- `distill_reindex` on the three stems above indexes 20/18/9 chunks (matching
  `hs distill diagnose`), and they become searchable via `distill_search`.
- A regression test embeds a known glm_ocr markdown fixture and asserts
  `chunks_indexed == diagnose chunk count`.
- Sweep `embedding_skipped` for other docs whose `diagnose` yields >0 chunks and
  re-embed them.

### P1-14. Downloader saves the repository landing page when `download_urls` also holds a direct PDF (2026-07-29)
**Motivation:** Two abstract-only stubs were ingested, converted by `html-parser` in ~0.005 s / "1 page", embedded, and then had to be deleted by hand: `10.1109_tvcg.2009.113` (UFRGS Lume "Visualizar item" page) and `10.34726_hss.2014.27898` (TU Wien reposiTUm record page). Neither is a paywall or an anti-bot interstitial, so `is_paywall_html` (rc.349) and both `hs pipeline purge-skipped` / `purge-poisoned` signature lists miss them — they are *successfully retrieved wrong documents*. The catalog for the first one lists `download_urls: ["http://hdl.handle.net/10183/27630", "https://lume.ufrgs.br/bitstream/10183/27630/1/000751721.pdf"]`: the direct PDF was known and the handle redirect was chosen anyway. The TU Wien record page likewise advertises a 32.86 MB PDF that was never fetched. These land in Qdrant as 1–4 chunk documents whose text is repository chrome plus an abstract, which is exactly the low-value noise `distill_search` should never return.
**Change:** when a candidate URL set contains both a landing/handle URL and a direct PDF URL, order direct-PDF candidates first. After fetching HTML, require a positive "this is a full text" signal before accepting it as the paper — reject a document whose extracted body is dominated by repository navigation chrome, or that carries a link to a PDF it did not follow. Fail loudly (no catalog row) rather than storing a landing page as the paper; a stub row is the degraded-substitute pattern the ONE PATH rule forbids.
**Acceptance:** re-downloading `10.1109/tvcg.2009.113` fetches `.../000751721.pdf`, not the handle page. A landing page with no reachable full text produces no `papers/`, `markdown/`, or `catalog/` object at all. Neither stem reappears via `hs pipeline catch-up`.

### P1-1. Replace mutex-unwrap with error propagation
**Motivation:** Poisoned-mutex panics cascade in long-running processes.
**Scope:**
- `hs-common/src/logging/spool.rs:42, 46, 50, 57, 102, 109`
- `crates/hs-distill/src/adaptive_batch.rs:127, 140, 157, 237`
- `crates/hs-gateway/src/oauth.rs:169, 189, 281, 453`
- `crates/hs-gateway/src/enrollment.rs:45, 80`
**Change:** Every `.lock().unwrap()` → `.lock().map_err(|e| ...)?`. Callers return error (500 in gateway). No `.expect("poisoned")` — it is not acceptable for a single poisoned mutex to kill the gateway for all tenants.
**Acceptance:** `rg 'lock\(\)\.unwrap\(\)|lock\(\)\.expect' crates hs-common` returns zero matches.

### P1-2. Replace HTTP-header and URL parse unwraps in auth
**Motivation:** `hs-common/src/auth/client.rs:167, 175` — `parse().unwrap()` on Authorization and CF-Access header values panics on malformed tokens.
**Scope:** `hs-common/src/auth/client.rs`
**Change:** Use `.parse()?`. Add token-shape validation at the boundary (token loader), so by the time headers are built, invalid tokens are impossible.
**Acceptance:** No `.unwrap()` on `HeaderValue::from_str` or `.parse()` in the auth module.

### P1-3. Remove panics from S3 signer
**Motivation:** `hs-common/src/storage/s3.rs:102, 104, 105, 136, 147` — five `.expect()` on URL parse, host extraction, and HMAC key derivation. A malformed endpoint config panics per request.
**Scope:** `hs-common/src/storage/s3.rs`
**Change:** Signer returns `Result`. Validate endpoint + bucket at config-load time so request-time signer inputs are always well-formed. Delete the panics; do not add a fallback path.
**Acceptance:** `rg '\.expect\(' hs-common/src/storage/s3.rs` returns zero matches in non-test code.

### P1-4. Fix auth token refresh race
**Motivation:** `hs-common/src/auth/client.rs:98, 118` — two concurrent `get_access_token()` calls both see expired and both call `refresh_access_token()`, wasting a refresh and risking a token storm.
**Scope:** `hs-common/src/auth/client.rs`
**Change:** Single in-flight refresh: use `tokio::sync::OnceCell` per token, or a `Mutex<Option<Shared<Future>>>` where concurrent callers await the same refresh. No "retry on failure" fallback; if the refresh errors, all waiters see the error.
**Acceptance:** Under 100 concurrent `get_access_token` callers with an expired token, exactly one HTTP refresh is issued.

### P1-5. Remove remaining JSON-serialize-or-empty fallbacks
**Motivation:** Sites publish empty bytes on serde failure, which gets consumed downstream as "empty event."
**Scope:**
- `hs-common/src/inbox.rs:110` (covered in P0-11, leave reference)
- `crates/hs-scribe/src/event_watch.rs:250`
- `crates/hs/src/pipeline_cmd.rs:188, 339`
- `paper/src/providers/downloader.rs:398`
**Change:** Replace `.unwrap_or_default()` with `?`. No "publish empty on error" — if the payload can't be serialized, the publish fails and the caller retries or bails.
**Acceptance:** `rg 'to_vec.*unwrap_or_default|to_string.*unwrap_or_default' crates hs-common paper` returns zero matches.

### P1-6. Fix tokio::spawn fire-and-forget leaks
**Motivation:** Handler panics silently vanish.
**Scope:** `crates/hs-scribe/src/server.rs:289`, `crates/hs-scribe/src/event_watch.rs:319`
**Change:** Capture the `JoinHandle` and either await it in a supervisor task that logs on panic, or wrap the closure in `AssertUnwindSafe(...).catch_unwind().await` and log. No silent drop.
**Acceptance:** A forced panic in either handler produces a logged event at `error` level.

### P1-7. Fix SIGKILL escalation gaps in process-kill paths
**Motivation:** `crates/hs/src/serve_cmd.rs:313-315, 324-325` sends SIGTERM then polls 50×100ms with no SIGKILL fallback. Line 284 already has the right pattern.
**Scope:** `crates/hs/src/serve_cmd.rs`, `crates/hs/src/distill_cmd.rs:332-337`
**Change:** Unify on a single `kill_with_escalation(pid, grace: Duration) -> Result<()>` helper in `hs-common`. After grace, send SIGKILL, then confirm. Also add the "is this PID actually the expected binary" check (read `/proc/<pid>/exe` on Linux, `ps` on macOS) before any kill.
**Acceptance:** All process-kill paths call the one helper. Killing the wrong PID is impossible by construction.

### P1-8. Remove model-input-index panics
**Motivation:** `crates/hs-scribe/src/models/layout.rs:118-132` indexes `inputs[0..2]` with `.unwrap_or_else(|| inputs[N].clone())` — panics if the ONNX model has <3 inputs.
**Scope:** `crates/hs-scribe/src/models/layout.rs`
**Change:** Validate input count at model load; return `Err` with a readable message if the count is wrong. Delete the index-with-fallback pattern.
**Acceptance:** Loading a malformed model returns a clear error at construction; no panic path from any runtime call.

### P1-9. Fix eval metric NaN panic and guard-mismatch unwraps
**Motivation:**
- `crates/hs-scribe/src/eval/metrics/edit_distance.rs:716` — `partial_cmp().unwrap()` panics on NaN.
- `crates/hs-scribe/src/eval/metrics/composite.rs:72` — unwrap after weak guard.
- `crates/hs-scribe/src/eval/datasets/fintabnet.rs:169-170` — `min().unwrap()` on possibly-empty vec.
**Scope:** the three files above.
**Change:** Use `total_cmp` for floats. Rearrange composite to explicit pattern match. Add length check in fintabnet before `min()`. No fallback 0.0 — empty input returns `Err`.
**Acceptance:** Running the eval harness on a row with NaN or empty fields produces a clean error, not a panic.

### P1-10. Fix Qdrant conversion panic
**Motivation:** `crates/hs-distill/src/qdrant.rs:138` — `payload.try_into().unwrap()` panics on malformed metadata.
**Scope:** `crates/hs-distill/src/qdrant.rs`
**Change:** Return `DistillError::Qdrant(...)` on conversion failure.
**Acceptance:** No `.unwrap()` on `try_into` in the distill crate.

---

## P1 — Data-integrity silent-failure cleanup

### P1-11. Delete `.ok()` / `let _ =` silent-error patterns in catalog/storage paths
**Motivation:** Broad-sweep of error-swallowing that hides state drift. Each site is individually small; together they make the system lie about its own state.
**Scope:**
- `crates/hs/src/scribe_cmd.rs:819-820, 824` — `fs::remove_file` swallowed in `cmd_clean_junk`.
- `crates/hs/src/scribe_inbox_install.rs:108, 111, 121, 147, 238, 263, 274` — seven `let _ =` on dir-create + launchctl/systemctl.
- `crates/hs/src/distill_cmd.rs:756` — index status write.
- `hs-common/src/storage/mod.rs:79-90, 114-115` — mtime reads.
- `paper/src/providers/downloader.rs:255, 264, 271, 278, 285` — per-provider resolver errors.
**Change:** Every site either propagates the error or logs at `warn!`/`error!` with the underlying cause before continuing. No bare `let _` or `.ok()` discarding a `Result` whose failure changes user-observable behavior.
**Acceptance:** Per-file review shows every remaining `.ok()` / `let _` either (a) applies to a value that genuinely has no observable side-effect or (b) has a nearby `tracing::warn!` capturing the reason.

### P1-12. Fix catalog update read-modify-write atomicity
**Motivation:** `hs-common/src/catalog.rs:193, 221, 454, 483, 540, 551, 573` — seven `update_*_catalog` functions read with `.unwrap_or_default()` and blind-overwrite. Concurrent updates erase sections (embedding metadata wiped by a conversion update).
**Scope:** `hs-common/src/catalog.rs`
**Change:** Single `update_catalog<F>(stem, section_update_fn: F)` kernel that reads, applies the closure, writes. Use storage conditional-put (S3 If-Match, local temp-file+rename) to detect concurrent modification and retry. All seven update functions call the kernel.
**Acceptance:** Concurrent `update_conversion` + `update_embedding` on the same stem never loses either section.

### P1-13. Replace `dirs::home_dir().unwrap_or_default()` with fail-loudly
**Motivation:** `hs-common/src/storage/config.rs:23, 81, 84` — missing home dir silently defaults to empty path; catalog entries end up under `./`.
**Scope:** `hs-common/src/storage/config.rs`, and `crates/hs/src/upgrade_cmd.rs:366-368`.
**Change:** Return `Result`; if `dirs::home_dir()` is `None`, error with a clear message. No default empty path.
**Acceptance:** Running under an environment with no home dir produces a clear error at startup, not silently wrong paths.

---

## P0 — rc.314 self-test follow-ups (2026-05-02 18:19Z)

### P0-17. `papers.ingested` publish failure leaves catalog stamped + pipeline stuck
**Motivation:** `paper/src/providers/downloader.rs:394-406` writes the PDF to storage, then publishes a `papers.ingested` NATS event so scribe picks it up. **Publish failure is a `tracing::warn!` and continues** — the catalog row is already stamped `downloaded:true` at this point. If the publish fails (NATS partition, JetStream auth glitch, transient broker outage) the doc sits on disk forever with no convert event. Confirmed by `10.1080_00224490902747222` in the 2026-05-02 self-test: 386KB downloaded, no `conversion` and no `conversion_failed` field, both scribes idle. Direct ONE-PATH violation: silent degradation when the primary path can't produce a usable result.
**Scope:** `paper/src/providers/downloader.rs:394-406` — the `events.publish("papers.ingested", ...)` block.
**Change:** Promote publish failure to `Err`. The catalog row write happens AFTER this in the MCP handler (`crates/hs-mcp/src/main.rs:533+`), so propagating the error means the catalog isn't stamped — the operator sees a clear download failure they can retry, instead of a doc that's invisible to convert. If a `reconcile`-style backfill is genuinely needed, it should be a separate explicit code path with its own diagnostic, not a quiet warn.
**Acceptance:** With NATS unreachable, `paper_download` returns `Err` and **no** catalog row is written. Storage is left as the only side effect (which a subsequent `disk_no_catalog` repair direction can pick up).

### P0-18. `paper_download` accepted unknown content-type as PDF (PARTIAL FIX 2026-05-02)
**Motivation:** Pre-fix, `paper/src/providers/downloader.rs:367-370` had a fallthrough — anything that wasn't `%PDF-` and wasn't HTML got stored under the original PDF key with comment "might be a valid binary format." Triggered F1 in the 2026-05-02 self-test: a 386KB JPEG of a graphical abstract was stored as `papers/10/10.1016_j.neubiorev.2021.07.036.pdf`, then convert dispatched 79 times before being marked `conversion_failed: unsupported_content_type:binary`. Same pattern produced 0-byte stubs (sha256 = SHA-256 of empty string).
**Fixed in-session:** added `MIN_PDF_BYTES=100` gate AND replaced the unknown-content-type fallthrough with `PaperError::NotFound("downloaded body is neither PDF nor HTML")`. Both deployed in `~/.local/bin/hs.rc314-self-test-fixes` on `big`. Tests pass. **Follow-up:** content-type sniff should also reject non-`%PDF-` binaries that happen to be ≥100 bytes (the JPEG case). Currently the magic-byte check at line 357 still passes anything that doesn't start with `%PDF-` to the HTML path; if it isn't HTML either, my new error fires. So the fix should already catch JPEGs. Add a regression test covering JPEG-bytes-named-as-PDF; tracking that as P1 follow-up.

### P0-19. `distill_scan_repetitions` blind spot — measures truncation count, not run length (per BACKLOG P0-14)
**Motivation:** Confirmed in the 2026-05-02 self-test: `10.48550_arxiv.2312.10997` had clear `valvalvalvalval...` and `the retrieval process is that the retrieval process is that...` repetition in indexed chunks but `distill_scan_repetitions` did NOT flag it (truncation count was below threshold 20). The `longest_repeated_run_bytes` helper exists at `crates/hs-scribe/src/postprocess.rs:160` and is already called by the convert-side QC gate, but the scanner at `crates/hs-mcp/src/main.rs:2145-2210` never invokes it. **Same gap as P0-14 in the rc.308 follow-ups — re-prioritize.**
**Scope:** `crates/hs-mcp/src/main.rs:2145-2210` (the scanner handler).
**Change:** Compute `longest_repeated_run_bytes(&original)` per markdown, return both `truncations` and `longest_run_bytes` per flagged doc. Flag if `truncations > truncation_threshold` OR `longest_run_bytes > longest_run_threshold` (default 1024, matching the convert-side QC gate's `QC_LONGEST_RUN_BYTES_MAX`). Operationally, this scanner becomes a corpus-cleanup tool for already-poisoned docs.
**Acceptance:** Re-converting `10.48550_arxiv.2312.10997` and re-running the scanner flags it.

---

## P1 — rc.335 deploy follow-ups (2026-06-11)

### P1-D1. `hs upgrade` restart list misses `hs-serve-scribe-olmocr`
**Motivation:** rc.335 deploy on `big` restarted hs-serve-scribe / hs-serve-distill / hs-serve-mcp but left the second scribe unit serving rc.334 until a manual `systemctl restart hs-serve-scribe-olmocr`. Same class as the original "hs upgrade skips server binaries" defect: the restart list is hardcoded instead of derived.
**Change:** restart every `hs-serve-*` unit (glob the unit names, or register units at install time), not a fixed list.
**Acceptance:** after `hs upgrade` on a host with both scribe units, `curl :7433/health` and `curl :7435/health` both report the new version with no manual step.
**Still open (rc.340, 2026-06-16):** confirmed on every rc.338 → rc.340 deploy on `big`. The gap also skips **`hs-serve-olmocr-vllm`** (the vLLM backend), not just `hs-serve-scribe-olmocr` — each deploy required a manual `sudo systemctl start hs-serve-olmocr-vllm hs-serve-scribe-olmocr`. Broaden the derive to cover ALL `hs-serve-*` incl the vLLM unit (`restart_cmd.rs` hardcodes `["scribe","distill","mcp"]`).

### P1-D2. big_mac deploy blocked: reboot + remount + root-owned mountpoint stub
**Motivation:** big_mac is wedged on ephemeral-port exhaustion (31k TIME_WAIT leaked by the now-disabled `scribe-autotune` LaunchAgent polling ollama; 86-day uptime) so NFS remount and `hs upgrade` downloads fail with EADDRNOTAVAIL. Additionally `/Volumes/home-still` now exists as a root-owned empty dir (sudo mkdir during recovery), so `hs` panics with `Permission denied` opening its log spool. Host stuck on rc.326.
**Change:** operator: `sudo reboot`, then `sudo bash /tmp/mount-hs.sh` (remount), then `hs upgrade --pre --yes`. Consider: hs on macOS should not hard-depend on an NFS-backed log spool dir at startup (local spool + shipper).
**Acceptance:** `ssh big_mac hs --version` → rc.335; no hung `hs` processes; mount healthy.

### P1-D3. `scribe-autotune` still running on mac_air — same socket-leak class that wedged big_mac
**Motivation:** `launchctl list` on mac_air shows `com.home-still.scribe-autotune` (PID 1781) alive. On big_mac the identical agent leaked ~1000 conn/s to `localhost:11434` until the 16K ephemeral-port range was exhausted, taking down all outbound TCP. mac_air has ollama and the same KeepAlive plist.
**Change:** either fix the autotune client to reuse one HTTP connection (it builds a fresh connection per poll) or disable the agent on hosts that aren't scribe workers. Decide whether autotune is part of the chain topology at all.
**Acceptance:** `netstat -an | grep -c TIME_WAIT` on mac_air stays <1k over a day, or the agent is removed.

### P1-D4. Watcher topology: user-unit daemons + manual `hs scribe watch-events` fight over the durable consumer
**Motivation:** big runs `hs-scribe-watch-events` / `hs-distill-watch-events` as user-scope systemd units (Restart=always). `ensure_consumer` deletes-then-recreates the durable on connect, so any second watcher instance (e.g. an operator running `hs scribe watch-events` by hand) kills the unit's consumer and vice-versa ("consumer deleted" churn), and each recreate RESETS JetStream delivery counts — a max_deliver-exhausted poison message comes back to life. Observed live during the rc.335 deploy (mcconnell resurrected).
**Change:** detect an existing live consumer with a different instance and refuse to start (fail loudly: "watcher already running"), or make ensure_consumer update-in-place instead of delete-first.
**Acceptance:** starting a second watcher instance on a host with the unit running exits with a clear error; delivery counts survive watcher restarts.
**ESCALATE TO P0 (2026-07-29):** this is not an operator-error edge case — it is the steady-state fleet topology, and it has been silently destroying the embed leg. `big` runs `hs-distill-watch-events` (user unit, concurrency=8) and `bmb` runs `com.home-still.distill-watch-events` (LaunchAgent, concurrency=6). Both bind durable `distill-workers` on stream `SCRIBE`; each one's `ensure_consumer` delete-then-recreate evicts the other, which restarts and evicts back. Measured: big's restart counter at **11,920**; bmb loops every ~11 s; the durable's `created` timestamp advances to *now* on every poll. Neither watcher ever consumes a message. `scribe.completed` events are therefore never indexed by the daemon path — the embed backlog (`markdown` 8075 vs `embedded_documents` 7766) is the residue. Both daemons log only `WARN jetstream delivery error error=consumer deleted`, and neither systemd nor `hs status` reports a fault: `hs status` showed `distill_instances[0].healthy: true` throughout.
**Additional acceptance:** two watcher daemons on different hosts pointed at the same distill server either (a) share the durable as a real queue group with no recreate, or (b) the second one fails loudly at startup. A watcher that cannot bind its consumer must exit non-zero with a distinct message, not spin on `Restart=always` — 11,920 silent restarts must be impossible.

## P1 — rc.340 deploy follow-ups (2026-06-16)

### P1-D5. `hs upgrade` on a systemd-native host starts conflicting podman containers
**Motivation:** `big` runs scribe via the systemd-native `hs-serve-*` units, but `hs upgrade` *also* runs `podman-compose -f ~/.home-still/docker-compose.yml up -d`, starting `home-still_scribe_1` + `home-still_qdrant_1`. The scribe container binds `0.0.0.0:7433` — the exact port systemd `hs-serve-scribe` already holds — so on every rc.338 → rc.340 deploy the container either loses the race (logs `rootlessport ... bind: address already in use`, harmless noise) or wins it: on rc.338 the container squatted 7433 with a **stale `:latest` (rc.335) image** while systemd `hs-serve-scribe` crash-looped on `Address already in use`, serving the OLD binary until an operator ran `podman stop home-still_scribe_1`. The container path is half-wired and fights the native services it's meant to replace. (Qdrant has the same dual-path risk — a container qdrant bound 6333-6334 alongside whatever served before; left untouched during recovery, but unverified.)
**Change:** `hs upgrade` must not run the compose stack on hosts that run the native systemd units — detect the native units and skip compose, or make the compose step explicitly opt-in per host. On `big`, scribe/distill are native; only the Pis use `ghcr.io/home-still/hs-scribe-server` containers. Closely related to **P1-D1** (both are `hs upgrade` doing the wrong thing per host).
**Acceptance:** `hs upgrade` on `big` leaves systemd `hs-serve-scribe` holding `:7433` on the new version, starts **no** `home-still_*` podman container, and needs no manual `podman stop` / port-clash recovery.

---

## P0 — rc.334 scribe-chain follow-ups (2026-06-10)

> **rc.335 note (2026-06-11):** P0-20's *instance* is resolved — the Russian
> mcconnell PDF was dropped per decision, and the redelivered message TERMed
> with an honest `conversion_failed: source_missing` stamp under rc.335's
> single classify table. The *general* defect (max_deliver exhaustion leaves
> no stamp) remains open below. P0-21 (3600s dispatch cap) also remains open.

### P0-20. JetStream `max_deliver=5` exhaustion is silent — no catalog stamp, doc vanishes from the pipeline
**Motivation:** `mcconnell_code_complete_2nd` (889-page test book) burned all 5 deliveries on the `scribe-workers` consumer overnight (last NAK 2026-06-10 06:00:42Z, `backoff_secs=30` logged — then nothing, ever). JetStream stops redelivering after `max_deliver` with no notification to the consumer; the catalog YAML still reads `{}` — no `conversion_failed`, no `attempts_log`. The doc is invisible to every repair direction (`stuck_convert` can't see it because there's no stamp at all). Direct ONE-PATH/fail-loudly violation: the transient-NAK path assumes redelivery is infinite, but the broker caps it.
**Scope:** `crates/hs/src/scribe_cmd.rs` (`cmd_watch_events` transient/NAK branch) + the consumer config site that sets `max_deliver`.
**Change:** Read `num_delivered` from the message metadata; when `num_delivered == max_deliver` (final delivery) and the chain outcome is Transient, do NOT NAK — stamp `conversion_failed: max_deliveries_exhausted` with the full `attempts_log`, then ACK terminally. The operator sees the failure in the catalog instead of archaeology in jsz.
**Acceptance:** With all chain backends unreachable and a 1-message stream, after 5 deliveries the catalog carries `conversion_failed: max_deliveries_exhausted` + 5×N attempt entries, and `nats consumer report` shows 0 pending / 0 ack-pending.

### P0-21. Page-scaled dispatch timeout caps at 3600s — book-length GLM converts structurally cannot finish
**Motivation:** mcconnell's delivery-5 GLM leg on big started 06:00:38Z−3600s, was actively converting pages at 05:54Z (layout-model WARNs in `hs-serve-scribe` journal; watchdog did NOT fire — the 4500s threshold patch held), and was killed at exactly 06:00:38Z by the dispatcher's own `timeout_secs=3600` ceiling (journal: `dispatching pdf to scribe with page-scaled timeout … pages=889 timeout_secs=3600`). At GLM's observed page rate, 889 pages needs multiple hours; the cap guarantees the timeout → `vlm_transport_error` → escalate, wasting a full hour of GPU per delivery and making GLM permanently unable to convert any book-length PDF through the watch-events path.
**Scope:** the page-scaled timeout computation in `crates/hs/src/scribe_cmd.rs` (the `timeout_secs` clamp).
**Change:** Raise/remove the 3600s clamp so the per-page scaling actually governs (e.g. `pages × per_page_secs` with a much higher absolute ceiling), and keep the hs-scribe-watchdog stall threshold consistent with it (currently hand-patched to 4500s on big — `~/.local/bin/hs-scribe-watchdog`, NOT in repo; check it in or fold it into `hs`).
**Acceptance:** An 889-page PDF dispatched through watch-events gets a deadline ≥ its realistic GLM convert time, and the watchdog does not kill the in-flight convert.

---

## P1 — rc.314 self-test follow-ups (2026-05-02)

### P0-15. Mass cascade of `papers/.quarantine/*` events floods scribe-watch with permanent failures
**Motivation:** During the 2026-05-02 recovery investigation, journal output from `hs scribe watch-events` (PID 2077303) shows bursts of 25+ permanent failures in a single second (12:45:43 UTC), most for keys under `papers/.quarantine/W2/...`, `papers/.quarantine/W3/...`, `papers/.quarantine/10/...`. Per CLAUDE.md the bad-PDF folder is `corrupted/` — `.quarantine/` is a parallel mechanism produced by `hs migrate quarantine-bad-content` (`crates/hs/src/migrate_cmd.rs:614`). S3 has 17 files under `papers/.quarantine/` but the consumer is processing 25+ events for them in one tight burst, meaning either (a) NATS is redelivering events that should have been ACK'd terminally, or (b) some publisher (`hs pipeline catch-up`, `hs pipeline rebuild`, the inbox watcher, or one of the `bus.publish("papers.ingested", ...)` sites at `pipeline_cmd.rs:187,338`, `migrate_cmd.rs:599`, `scribe_cmd.rs:198`, `mcp/main.rs:1340`, `inbox.rs:116`) is emitting them in a loop. Either way the consumer is doing GPU work for files that should never re-enter the convert path.
**Scope:**
- `crates/hs-scribe/src/event_watch.rs` — the `run_subscriber` loop's NATS-ACK behavior on `HandlerError::Permanent` (verify the message is *terminally* acknowledged and not redelivered).
- All `bus.publish("papers.ingested", ...)` call sites listed above — confirm none of them filters on `.quarantine/` exclusion.
- `crates/hs/src/migrate_cmd.rs:441-442` already filters `.quarantine/` for the migrate scan; ensure every other walker (catch-up, rebuild, inbox sweeper) does the same.
**Change:** ONE PATH. The convert pipeline must never see `.quarantine/` keys. Add a single guard at the publish boundary (in the helper that constructs `papers.ingested` payloads) that drops any key matching `.quarantine/` or `corrupted/` (the canonical bad-PDF folder name). If a publisher tries to enqueue such a key, that's a caller bug — fail loud at the publish site.
**Acceptance:**
- After landing, `journalctl --user _PID=<watch-events pid> | grep '/.quarantine/'` returns zero lines over 24h.
- `papers/.quarantine/` directory is either renamed to `corrupted/` (per the project standard) or its files moved there; `.quarantine/` no longer exists in S3.

### P1-19. Pipeline drift not draining — caused by rc.310 QC gate aggressively rejecting + .quarantine cascade (corrected diagnosis 2026-05-02)
**Motivation:** Original P1-19 hypothesis was wrong. `total_conversions` IS a completion counter (server.rs:251 ticks on `Ok(Ok(md))`), markdown writes ARE protected by `?` propagation (event_watch.rs:332-334), and `conversion_failed` stamps DO land in S3 (verified via `aws s3 ls catalog/10/`). The real reason `markdown` count has been frozen at 3547 since 2026-04-29 and `catalog_recent` stops at 2026-05-01T11:47:
1. The rc.310 P0-12 QC gate (`postprocess.rs:107-148`) is rejecting most converts as `RejectLoop` on aggressive thresholds — `truncations=5, longest_run=16B` triggered a permanent reject in this session. The bad-pages condition `bad_pages.saturating_mul(100) > total_pages.saturating_mul(QC_BAD_PAGE_RATIO_PCT)` with `QC_BAD_PAGE_RATIO_PCT=10` rejects any paper where >10% of pages have ANY truncation activity, which is a very tight bound for normal scientific PDFs that often have minor repetition that gets cleaned cosmetically.
2. The `.quarantine/` cascade (P0-15 above) is consuming most of the throughput, producing dozens of permanent failures that don't grow the markdown count.
3. Net effect: out of every batch of conversions, ~all are getting RejectLoop or PDF-format-error stamps, so `markdown` doesn't grow, drift = `documents − markdown − in_flight` keeps rising as new papers arrive.
**Scope:**
- `crates/hs-scribe/src/postprocess.rs:53-78` — the QC threshold constants (`QC_ABSOLUTE_MAX=20`, `QC_PER_PAGE_MAX=3`, `QC_BAD_PAGE_RATIO_PCT=10`, `QC_LONGEST_RUN_BYTES_MAX=1024`).
- The corpus of papers stamped with `conversion_failed: vlm_repetition_loop` since rc.310 — needs a sample inspection to know whether the rejections are real loops or false-positive over-strict rejections.
- The interaction with **P0-15** (.quarantine cascade) — once that's fixed, drift may largely heal on its own as the consumer's slots free up for legitimate work.
**Change:** This is a product/operations decision, not a clear bug fix. Two paths to consider, in order of cheapness:
1. **First fix P0-15.** With the .quarantine flood gone, the QC gate's blast radius shrinks, and we can measure the legit rejection rate. If it's tolerable (e.g. <5% of converts), the threshold is fine.
2. **Then look at QC tuning.** If the legit rejection rate is too high, raise `QC_BAD_PAGE_RATIO_PCT` (10 → 20-30) or change the gate to require `total > QC_ABSOLUTE_MAX` AND `bad_pages_pct` together rather than either alone. Run the existing tests at `postprocess.rs:501-681` to verify no regression on the synthetic loops the rc.310 fix targeted.
**Acceptance:**
- Pipeline drift falls to ≤ 3 within 30 minutes of the next full convert pass.
- New `Convert` rows appear in `catalog_recent` at a rate matching `scribe_health.total_conversions`.

### P0-16. `hs status` TUI silently renders fake-empty dashboard on MCP failure (auth expired, gateway down, etc.)
**Motivation:** When the cloud-token at `~/.home-still/cloud-token` expires (refresh token TTL), `hs status` shows a TUI with `Documents …`, `Watcher ○ stopped`, `Qdrant ○ stopped`, and `History: No activity yet` — visually identical to "the cluster is dead." The actual error from the underlying call is `Token refresh failed (401 Unauthorized): Refresh token expired — re-enroll with hs cloud enroll`. The silent fallback at `crates/hs/src/status_cmd.rs:127-147` constructs a default-zero `DashboardData` with `qdrant_healthy: false, watcher: WatcherInfo::Stopped` whenever `collect_data_via_mcp()` returns `Err(_)`. The comment claims "zeros are accurate ('we don't know yet') rather than confidently wrong" — but the rendered dashboard *is* confidently wrong: it asserts that every service is `stopped`, which is not what "we don't know" looks like in the UI. Direct violation of the project's ONE PATH / fail-loudly non-negotiable.
**Scope:** `crates/hs/src/status_cmd.rs` — at minimum the fallback at lines 127-147, the `DashboardData` struct (lines 17-51), and the render entry (`fn render` at line 349). The text/JSON one-shot paths (`run_oneshot_text` line 821, `run_oneshot_json` line 811) propagate errors correctly via `?` — only the TUI swallows them.
**Change:** Add `error_message: Option<String>` to `DashboardData`. On MCP failure, populate it with the underlying error chain (`format!("MCP unreachable: {e:#}")`). In the renderer, when `error_message.is_some()`, replace the entire content area with a red-bordered banner showing the error and a hint to run `hs cloud enroll` if the message contains `401` or `Refresh token`. Do not also render the empty pipeline rows beneath — surface the error instead of pretending we have data.
**Acceptance:**
- With an expired cloud-token, `hs status` shows a red banner: `MCP unreachable: ... 401 Unauthorized ... — run \`hs cloud enroll\`` and no other panels.
- With MCP healthy, the dashboard renders normally (no regression).
- `hs status --output json` still propagates the error verbatim (already correct).

---

## P1 — rc.308 self-test follow-ups

### P1-14. Replace `distill_reconcile` facet aggregate with bounded scroll (F4)
**Motivation:** `distill_reconcile` with default `limit=100000` times out the 4-min MCP deadline on a 103k-point collection. `limit=5000` (saturating at the 3399 actual doc-count) returns in seconds. Hypothesis "pagination tail-chases past end of collection" was wrong — verified by file read. `crates/hs-distill/src/qdrant.rs:347-352` uses `.facet(...limit=100000, exact=true)` — single synchronous aggregate query, no scroll loop. With `exact=true` Qdrant must walk the entire payload index; with smaller limits it can stop early.
**Scope:** `crates/hs-distill/src/qdrant.rs` (`list_doc_ids`, `distinct_doc_count`), MCP wrapper in `crates/hs-mcp/src/main.rs`.
**Change:** Replace facet with a bounded scroll over points (id + doc_id projection only), accumulating distinct doc_ids into a `HashSet`. Terminate on empty page. The MCP wrapper's `limit` becomes a scroll budget; document the new default.
**Acceptance:** `distill_reconcile` returns under 30 s on the full 103k-point collection with default arguments.

### P1-15. Add `hs catalog purge` CLI; close orphan catalog rows (F8)
**Motivation:** `catalog_repair.catalog_no_source` reports rows where `downloaded:true` but no PDF/HTML/EPUB exists on disk. The 2026-05-02 self-test shows 27 such rows (up from 2 in rc.308) — sample stems include `10.1007_978-3-319-20010-1`, `10.1016_0167-6423(90)90067-n`, `10.1016_j.csbj.2016.12.005`. Caused by manual file deletion without catalog cleanup. There is no symmetrical `hs catalog purge` CLI; only `hs distill purge` exists. Cross-references **P0-6** which removes destructive ops from MCP.
**Scope:** `crates/hs/src/catalog_cmd.rs` (or wherever the catalog subcommands live).
**Change:** Add `hs catalog purge <stem>` that atomically deletes the catalog YAML row and any orphan source files. CLI-only (per memory `feedback_destructive_ops_cli_only`); do NOT add a corresponding MCP tool. Also support a bulk mode driven by `catalog_repair`'s phantom list, since 27-by-hand is operator hostile.
**Acceptance:**
- `hs catalog purge 10.1007_978-3-319-20010-1` removes the row and any matching `papers/10/...` files.
- After purging the orphans surfaced by `catalog_repair`, `catalog_no_source: 0`.

### P1-16. Aggregate `scribe_health` across configured instances (F10)
**Motivation:** `scribe_health` queries one scribe instance; `total_conversions` is in-memory `AtomicU64` and resets on restart. After a successful round-trip on `.111` followed by a `scribe_health` call that resolved to `.110`, the response was `total_conversions: 0` — confusing to operators even though the per-instance reset behavior is documented at `client.rs:80-91`. Not a defect; an observability gap.
**Scope:** `crates/hs-mcp/src/main.rs` (`scribe_health` handler).
**Change:** Iterate all configured scribe instances; return per-instance counters AND an aggregate sum. Surface each instance's URL, version, and last-restart timestamp so operators can interpret a zero counter as "recently restarted." Matches the `system_status` model.
**Acceptance:** With `.110` restarted but `.111` healthy, `scribe_health` returns one entry per instance plus an aggregate; the aggregate reflects converts on either instance.

### P1-17. `paper_get` arXiv DOI: fan-out enrichment via arXiv-ID (F9)
**Motivation:** `paper_get(10.48550/arxiv.2312.10997)` returns `doi: null`, `cited_by_count: null`, while the same paper in `paper_search` carries full DOI and citation count from OpenAlex/S2. The arXiv-DOI shortcut at `paper/src/services/search.rs:147-157` correctly skips DataCite-DOI fan-out (Crossref/OpenAlex/S2 don't index DataCite arXiv DOIs — documented in the comment), but does not perform an arXiv-ID-keyed enrichment lookup that those providers DO support (`/works/arxiv:NNNN.NNNN` on OpenAlex, `/v1/paper/arXiv:NNNN.NNNN` on S2). Don't remove the early-skip rationale — it's correct for DataCite DOIs.
**Scope:**
- `paper/src/services/search.rs:147-157`
- `paper/src/providers/openalex.rs` and `paper/src/providers/semantic_scholar.rs` — add arXiv-ID lookup methods if missing
- Reuse `paper/src/aggregation/{dedup, merge}` — proven path used at `services/search.rs:195-198`
**Change:** After the arXiv `get_by_doi` returns, extract the arXiv ID and concurrently query OpenAlex + S2 by arXiv-ID for enrichment. Merge results with the existing dedup+merge_group path.
**Acceptance:**
- `paper_get(10.48550/arxiv.2005.11401)` returns `cited_by_count > 10000` and a populated `doi`.
- arXiv-DOI lookup latency increases (acceptable cost) but stays within the global `paper_search` per-call budget.

### P1-18. `paper_search` citation-sort: enforce title-presence floor (F5)
**Motivation:** `paper_search(query="retrieval augmented generation", date=">=2024", min_citations=10, sort=citations)` returns at position 2: `W2951534261` "Analysis of Points of Interests Recommended for Leisure Walk Descriptions" (1288 cites; OpenAlex entity-drift onto MS MARCO). The relevance score in `relevance.rs::relevance_score` weights term_coverage (40%) + phrase_score (30%) across both title and abstract, so abstract-only matches clear the `CITATION_SORT_MIN_RELEVANCE = 0.3` floor and the citation boost lifts them to the top. OpenAlex entity drift itself is upstream and out of scope; the defensible fix is to make the relevance floor title-aware.
**Scope:** `paper/src/aggregation/relevance.rs` (`relevance_score`).
**Change:** Add a title-presence floor: if fewer than 50% of query terms appear in the title, cap the score below `CITATION_SORT_MIN_RELEVANCE` regardless of abstract content. Single gate, applied in `relevance_score` itself. Don't add a second sort-time filter.
**Acceptance:** The cited query returns no off-topic high-citation papers in positions 1–10. Existing `target_paper_survives_citation_floor_even_with_fewer_citations` test still passes.

---

## P1 — 2026-07-15 ops follow-up (event-driven conversion silently halted)

### P1-19. `hs-scribe-watch-events` stays dead after a deploy/`hs restart` stops it — silent conversion halt
**Motivation:** On 2026-07-15 manual ingestion looked broken — files dropped in `papers/manually_downloaded/` were swept and published to `papers.ingested`, but nothing converted for ~2 days. Root cause: `hs-scribe-watch-events.service` (the NATS `papers.ingested` → scribe-pool consumer) was `inactive (dead)` since 2026-07-13 10:37, killed by SIGTERM — a clean stop, almost certainly a deploy / `hs restart`. The unit sets `Restart=always`, but that only recovers crash exits; a deliberate `systemctl stop` leaves it down permanently. The outage was **silent**: `hs status` showed "Scribe ● running" because that row reflects the scribe *server* (`hs-serve-scribe`), not the event consumer, and no catch-up timer was running (`hs-pipeline-catchup.timer` disabled). A manual `systemctl --user start` restored it and it drained the backlog immediately — so this recurs on every deploy that stops it without restarting.
**Scope:**
- The deploy/restart flow that issues the SIGTERM (`hs upgrade` / `hs restart` service management in `crates/hs/src/`).
- `hs status` health surface (server-liveness vs consumer-liveness conflation).
**Change:**
1. The deploy/`hs restart` path must restart every event consumer it stops (`hs-scribe-watch-events`, and any peer) and verify each via `is-active` post-deploy, failing loudly if one didn't come back. No "stopped and forgot".
2. `hs status` must show the scribe *consumer's* heartbeat as a distinct row (like the inbox watcher's `last_tick_seconds_ago`), not conflate it with the server's "running". A dead consumer must turn the dashboard red.
3. (Decide, separate) `hs-pipeline-catchup.timer` as a periodic source-scan re-queue is a hidden fallback that masks a dead consumer — a ONE-PATH smell. Either make it the intended one path or delete it; don't leave it as a silent backstop.
**Acceptance:**
- After `hs upgrade` / `hs restart` on any host, `hs-scribe-watch-events` is active, and a post-deploy check fails loudly if it isn't.
- `hs status` shows a scribe-consumer heartbeat that goes red within one tick of the consumer dying (repro: `systemctl --user stop hs-scribe-watch-events` → dashboard red).

---

## P1 — rc.353–355 fleet rollout follow-ups (2026-09-16)

### P1-20. `mac_air`'s scribe LaunchAgent is unloaded and its autotune agent exits 2 on every run
**Motivation:** `com.home-still.scribe` (plist `~/Library/LaunchAgents/com.home-still.scribe.plist`, `ProgramArguments = /Users/ladvien/.local/bin/hs-scribe-server --host 0.0.0.0 --port 7433`) has been **unloaded on mac_air** since the 2026-09-08 rc.352 rollout, when `launchctl` returned `Unload failed: 5: Input/output error` / `Load failed: 5: Input/output error` (see P0-17). The rc.353+ restart logic correctly ignores it — `launchctl list` never lists the label, so there is no loaded unit to kickstart — and the pool is unaffected because `mac_air` was commented out of `scribe.servers` on `big` back on 2026-04-24. So this is residue, not lost capacity: an installed LaunchAgent for a server nobody dispatches to, on a host that is otherwise current (`hs 0.0.1-rc.355`). The second half is noisier: `launchctl list` on mac_air shows `com.home-still.scribe-autotune` with last exit status **2** and `com.home-still.ollama` with last exit status **1**, both loaded and both not running — two agents failing on every launch with no operator signal. Only `io.home-still.scribe-inbox` and `com.user.rclone-nfsmount-home-still` are actually alive there.
**Scope:** `mac_air` host state only — `~/Library/LaunchAgents/com.home-still.scribe.plist`, `com.home-still.scribe-autotune.plist`, `com.home-still.ollama.plist`; `scribe.servers` in `big`'s `~/.home-still/config.yaml:83-91`.
**Change:** Decide mac_air's role once and make the host match it. If it stays out of the scribe pool, uninstall the three dead agents (`launchctl bootout` where loaded, then remove the plists) so `launchctl list` stops advertising units that cannot run. If it comes back, `launchctl load ~/Library/LaunchAgents/com.home-still.scribe.plist`, re-add `http://<mac_air>:7433` to `scribe.servers`, and fix the autotune agent's exit-2 cause rather than leaving it loaded-and-failing. A plist for a retired server is the same silent-drift pattern the ONE PATH rule bans — do not leave it installed "just in case".
**Acceptance:** `launchctl list | grep home-still` on mac_air shows only agents that are either running or intentionally on-demand; no label reports a non-zero last exit status on a steady-state host. If restored: `curl http://<mac_air>:7433/health` answers and the pool dispatches conversions to it.

### P1-22. `Check (windows-latest)` was red on every run — `windows-latest` is now the VS2026 image, which cannot build bundled DuckDB (2026-09-21)
**Motivation:** With the 152-commit `feat/distill-search-include-text` line merged to `main` (PRs #2 + #3, 2026-09-21), `ubuntu-latest` and `macos-latest` passed and `windows-latest` failed — on `main` (run 35615376153) exactly as on the branch (35607087681, 35612499194, and every Check run back to at least 2026-09-16). Not a Rust failure: `libduckdb-sys`'s bundled C++ build dies in `cc-rs` with `cl.exe` exit 2. `duckdb = { version = "1", features = ["bundled"] }` (workspace `Cargo.toml:33`) is pulled by `crates/hs`, `crates/hs-mcp` and `crates/openalex-ingest`, so `cargo clippy --workspace --exclude hs-scribe` could not even build on that runner. The root cause was already diagnosed and fixed **in the other workflow**: `.github/workflows/release.yaml:41-48` pins `windows-2022` with the comment that GitHub repointed both `windows-latest` and `windows-2025` to the VS2026 image (`windows-2025-vs2026`), whose MSVC removed `stdext::checked_array_iterator` — which DuckDB's vendored `fmt` still uses (it broke rc.336 + rc.337). `ci.yaml` never got the same pin, which is why the release workflow shipped `hs-v0.0.1-rc.356-x86_64-pc-windows-msvc.zip` green on the same day its Check leg was red. A permanently-red check trains everyone to merge past it — that is how `Check` stayed red 2026-09-08 → 2026-09-19 unnoticed.
**Scope:** `.github/workflows/ci.yaml:19` (the matrix `os` list); `.cargo/config.toml` (new).
**Change:** Pin the Check matrix's Windows leg to `windows-2022`, the same runner and for the same reason as `release.yaml` — one image policy across both workflows, not two — then fix whatever the pin uncovers rather than leaving a check that is expected to fail.
**Acceptance:** `gh run list --workflow ci.yaml --branch main` shows no `failure`, and the Definition of Done in this file's working rules can be read literally again.

**FIXED 2026-09-21 — two defects, not one.** (1) `ci.yaml`'s matrix is now `[ubuntu-latest, macos-latest, windows-2022]` with the image rationale inline; the job renames from `Check (windows-latest)` to `Check (windows-2022)`, and `main` has no branch protection so no required-check name needed updating. (2) The pin turned the compile error into a **link** error the VS2026 image had been masking: `LNK2019` on `RmStartSession` / `RmEndSession` / `RmRegisterResources` / `RmGetList`, referenced from `duckdb::AdditionalLockInfo` in `libduckdb-sys`'s bundled objects, which never emits a `cargo:rustc-link-lib=rstrtmgr`. `hs.exe` links because another dependency drags `rstrtmgr.lib` in — which is exactly why `release.yaml`'s `-p hs` build was green while the workspace Check leg was not — but the `openalex-ingest` and `paper` (`examples/sniff`, `snapshot_live`) test binaries are not so lucky. New `.cargo/config.toml` adds `rustflags = ["-C", "link-arg=rstrtmgr.lib"]` under `[target.x86_64-pc-windows-msvc]`, so the whole workspace links it once. Run 35621092679 on `main`: **all three legs green** — the first fully green Check since at least 2026-09-08.

---

## P1 — 2026-09-28 catch-up drain wedge

### P1-23. An escalation into a tier with no admitting host parks the handler for 65 min and wedges the consumer
**Motivation:** A one-shot `hs pipeline catch-up` on 2026-09-28 republished 60 events; `hs-scribe-watch-events` handled 8 and then made no progress for 20+ minutes with the process idle. Tier order is `[glm_ocr (bmb, cap 4), olmocr (big, cap 2)]`, global admission cap 6. Of 68 sources without markdown, 35 are ≤10 KB Elsevier `/retrieve/articleSelectPrefsTemp` meta-refresh stubs saved as `.html`. Each failed the indexable floor in the html-parser, and that message was unrecognised by `classify::classify_failure`, so the chain escalated it to the olmocr tier. big's scribe was refusing dispatch (`backend_unavailable`, VRAM gate), so `ServicePool::pick_server` polled it every 500 ms for `PICK_READY_TIMEOUT` = 3900 s while holding an olmocr permit. The rest queued on the tier semaphore, and all 6 global slots sat parked. After the deadline the events NAK and redeliver into the same trap (catalog rows show `attempts: 17`).
**Fixed in rc.357 (trigger only):** the floor error now names its converter, and parser output under the floor classifies `Permanent("empty_conversion")`. VLM output under the floor still escalates.
**Scope:** `hs-common/src/service/pool.rs` (`pick_server`, `PICK_READY_TIMEOUT`), `crates/hs/src/scribe_cmd.rs` (`cmd_watch_events` tier loop).
**Change:** Tell "tier saturated or unreachable" (poll, as before) apart from "every host answered and refused at its admission gate" (fail the pick at once). Treating *unreachable* as closed would be wrong: tier 1 is a laptop that sleeps, and with `max_deliver = 5` × 30 s NAK backoff (P0-20 still open) a fast-failing chain would drop events silently within minutes. When a gated tier is skipped, an earlier tier's outcome stands. A bmb transient therefore NAKs and retries. A bmb `Escalate` is stamped with its real reason and terminated, not re-run on glm for every redelivery.
**Acceptance:** With big's scribe reporting `backend_unavailable`, an event that escalates off bmb releases its olmocr permit within one probe interval, and a 60-event catch-up keeps draining on bmb.

**FIXED rc.358 (2026-09-28).** `ReadinessInfo::admits_work` (scribe: `backend_status != backend_unavailable`). `pick_server` returns `NoAdmittingHost` only when every probe answered and refused; any unreachable, busy, or admitting host keeps it polling. The chain skips such a tier without overwriting `last_err`. Pool tests pin fail-fast, the one-gated-host-still-waits case, and the unreachable-host-still-polls case.

### P1-24. `cargo test --locked -p hs-scribe --features server` died with SIGSEGV once on ubuntu-24.04 (rc.360 preflight, run 37326913748 attempt 1)
**Motivation:** The Gate's ubuntu leg exited 101 with `process didn't exit successfully: .../hs_scribe-550cd748ef92b3ea (signal: 11, SIGSEGV)` partway through the `config::tests::*` output of the `hs-scribe` lib test binary. Same tree passed on the main push run and on the re-run of the failed job; 6 consecutive local runs under `unshare -rn` (loopback only) on big passed 371/371, so it is intermittent and not reproduced. The CI leg downloads a real `libpdfium.so`, so the pdfium-backed tests execute there (they print SKIPPED without it).
**Scope:** `crates/hs-scribe/src/pdfium.rs` and the pdfium-backed tests in `crates/hs-scribe` (`pdf_meta.rs` and siblings); `.github/workflows/ci.yaml` ubuntu leg.
**Change:** Find which test thread faulted (re-run with `RUST_TEST_THREADS=1` and `--nocapture` on a loop under the CI libpdfium, chromium/7749, and capture a core), then fix the unsound concurrent use of the library. Do not add a retry to the Gate.
**Acceptance:** 50 consecutive CI-equivalent runs of the hs-scribe lib tests with libpdfium present without a SIGSEGV.


---

## P1 — branch/PR closeout (2026-09-19)

### P1-21. Scribe/distill GPU contention has no coordination path — salvaged question from the retired `feat/title-progress-bar` branch
**Motivation:** PR #1 (`feat/title-progress-bar`, merged 2026-09-19 as an `ours` merge — its history is recorded on `main`, its tree deliberately discarded because every feature had already landed via the `rc-231` line) carried exactly one file that never reached trunk: `hs-common/src/gpu_priority.rs`, 79 lines that made distill poll scribe's `.scribe-watch-status.json` every 5 s and yield the GPU while scribe had work queued. The file itself is not worth resurrecting — it hand-parsed `config.yaml` by line indentation to find `scribe.output_dir`, and a second arbitration path alongside the scribe autotuner is the ONE PATH violation this file's own rules ban. But the *question* it answered is still open in trunk: nothing coordinates the distill embedder and the scribe VLM for the same GPU. Today the only arbitration is indirect — the `home-still.slice` memory cap and the autotuner's concurrency ceiling — and the failure mode is documented in P0-15's motivation: pinning the distill embedder resident left 16.06 GiB free against vLLM's 16.49 GiB requirement, so olmocr could not start for four days.
**Scope:** `crates/hs-distill/src/embed/onnx.rs` (CUDA session lifetime), `crates/hs-scribe/src/server.rs` VLM slot accounting, the `home-still.slice` cgroup limits on `big`.
**Change:** Decide whether GPU arbitration is a real requirement or whether the slice cap is sufficient. If it is required, it belongs in one place with one owner — not a status file polled by the other side. Nothing here is urgent: trunk has run without it since rc.231.
**Acceptance:** Either a written decision that the slice cap is the one path (close this story), or a single arbitration mechanism with a test that proves distill cannot starve a scribe conversion of VRAM.

**DECIDED + IMPLEMENTED rc.356 (2026-09-21).** The slice cap alone is *not* sufficient — it bounds home-still's own footprint but says nothing about the foreign tenants that actually take the card (hand-launched `llama-server` instances, ollama, the TRELLIS MCP server: 23097 MiB of 24576 used at diagnosis time, none of it home-still's). The decision is an **admission gate, not a yield protocol**: neither service polls the other's status file. Each home-still GPU consumer asks the card directly, through the single source of truth `hs-common/src/gpu.rs`, whether the work it is about to start can start:
- scribe refuses dispatch unless its VLM model is resident or `vram_headroom_mb` is free (see P0-15);
- distill refuses to (re)load bge-m3 below `distill_server.embedding.vram_floor_mb` (default 5000) at both load sites — `OnnxEmbedder::new` and the lazy reload inside `embed_batch` — and names the holders in the error;
- `verify_cuda_probe` now gates on `hs_common::gpu::self_vram_mb()` (per-process) instead of whole-card `memory.used`, which on a contended card passed vacuously at 23 GB of *other* tenants' allocations.

Measured on `big` at 4932 MiB free: `hs-serve-distill` exits 1 with `gpu busy: 4932 MB free < 5000 MB required to load bge-m3; holders: pid=588655 8188MB llama-server; …` and retries on `Restart=always` until the card frees, instead of taking a CUDA OOM that poisons the ort session. After freeing VRAM it came up clean: `{"status":"ok","compute_device":"Cuda","version":"0.0.1-rc.356"}` with `CUDA verified: model loaded on GPU (Some(3390) MB VRAM, this process)`.

Eviction of *foreign* llama-swap models stays with `~/.local/bin/gpu-tenant`, whose job is now only that. It no longer stops home-still units (`units_for` deleted): a `tenant=coding` claim taken 2026-09-20T17:26 stopped `hs-serve-distill` and, never released, held it down for **14 hours** while `distill.servers` still advertised it. Claims are now leases — `gpu-tenant claim <tenant> [hours]`, default 4 h — and `gpu-tenant reconcile` (user timer `gpu-tenant-reconcile.timer`, `OnUnitActiveSec=5min`) drops an expired or leaseless claim. Its first run released the 14-hour claim: `released leaseless claim tenant=coding`. `SWAP_MODELS` was also wrong — it named `qwen3.8-27b`, which was never a model id, and omitted `blenderllm` and `qwen2.5-coder-7b-instruct` (~15 GiB each), so claim/release left them resident; it now matches `~/.home-still/llama-swap.yaml` exactly. `healthCheckTimeout` there dropped 600 → 180 s: with the scribe gate in front of it, a cold start is only attempted when it should succeed.

**Still open:** `OLLAMA_MAX_LOADED_MODELS=1` was not applied — `/etc/systemd/system/ollama.service.d/override.conf` needs root and `sudo` on `big` is password-gated for everything outside the `hs-serve-*` NOPASSWD rules. Nothing bounds how many models ollama stacks on the shared card (7394 MiB observed held by one idle model). Apply with:
```
sudo tee -a /etc/systemd/system/ollama.service.d/override.conf <<'EOF'
Environment="OLLAMA_MAX_LOADED_MODELS=1"
EOF
sudo systemctl daemon-reload && sudo systemctl restart ollama
```

---

## P2 — `hs status` user feedback correctness

### P2-1. Clamp `fmt_ago` to non-negative
**Motivation:** `crates/hs/src/status_cmd.rs:335` — clock skew produces "-45s ago."
**Change:** Clamp `num_seconds()` to >= 0 before formatting.
**Acceptance:** Future timestamps render as `0s ago`.

### P2-2. Fail loudly on broken `ProgressStyle` templates
**Motivation:** `hs-common/src/tty_reporter.rs:273, 278, 290, 295, 304, 309` — `unwrap_or_else(default_bar)` hides invalid template strings.
**Change:** `make_style()` / `make_spinner_style()` return `Result`. Templates are compile-time constants; failures are build/init bugs and must panic or bail at startup. Delete the default-bar fallback.
**Acceptance:** Corrupting any template string produces a loud startup error, not a silent blank bar.

### P2-3. Carry `embedding_skipped` across MCP hiccups
**Motivation:** `crates/hs/src/status_cmd.rs:989` — denominator of the Embedded% bar regresses when MCP collect briefly fails.
**Change:** One line: `data.embedding_skipped = new_data.embedding_skipped.or(data.embedding_skipped);`
**Acceptance:** Simulating a transient MCP error does not flicker the Embedded% bar.

### P2-4. Apply `.or()` retention to Watcher / Indexer / History rows
**Motivation:** `crates/hs/src/status_cmd.rs:973-990` — direct assignment on fresh data causes Watcher to flicker Stopped→Running on MCP hiccup.
**Change:** Use `.or()` retention consistently with other counters.
**Acceptance:** Under flaky MCP, Watcher/Indexer/History rows do not flicker between states on each failed tick.

### P2-5. Recompute status column widths on terminal resize
**Motivation:** `hs-common/src/tty_reporter.rs:313-316` — `bar_prefix_width()` captured once at `begin_stage`; stale after resize causes misaligned truncation.
**Change:** Recompute on SIGWINCH (listen via `signal-hook` or `crossterm::event::Event::Resize` in the TUI path). Invalidate cached prefix_width.
**Acceptance:** Resizing the terminal mid-conversion keeps bars aligned.

### P2-6. Derive status detail column width
**Motivation:** `crates/hs/src/status_cmd.rs:638-639` — `Constraint::Min(38)` is a magic number bumped from 14 in rc.303. Next new detail string might truncate silently.
**Change:** Either compute min-width from the set of possible detail strings at build time (const eval + test), or add a unit test that constructs every row variant and asserts each fits within the constraint.
**Acceptance:** Adding a new row with a longer detail string fails a test before merging.

### P2-7. Add sub-second precision to `fmt_ago` at zero-second bucket
**Motivation:** `crates/hs/src/status_cmd.rs:339` — two events 500ms apart both render "0s ago."
**Change:** When `secs == 0`, render `Nms ago` using `num_milliseconds() % 1000`.
**Acceptance:** Sub-second-spaced events render distinctly.

---

## P2 — Reliability follow-ups

### P2-8. Strict JSON deserialization in OCR providers
**Motivation:** `crates/hs-scribe/src/ocr/cloud.rs:36-37` and `crates/hs-scribe/src/ocr/openai_compatible.rs:66-68` use `body.get(...).and_then(...).unwrap_or("")` on response JSON — schema drift returns empty string silently.
**Change:** Define `serde`-derived response structs per provider. `serde_json::from_slice::<CloudResponse>(...)?` — schema drift errors at the boundary.
**Acceptance:** A provider returning an unexpected shape produces a clear deserialize error, not an empty markdown.

### P2-9. Strict chunker offset resolution
**Motivation:** `crates/hs-distill/src/chunker.rs:73-75` — `find()` with `unwrap_or(0)` corrupts line spans if the chunk text isn't in the source.
**Change:** Return `Err` if the chunk text isn't found. No `unwrap_or(0)`.
**Acceptance:** A synthetic test with mismatched chunk/source produces a loud error.

### P2-10. Fail loudly on eval metric errors; never substitute 0.0
**Motivation:** `crates/hs-scribe/src/eval/metrics/composite.rs:87`, `cdm.rs:116`, `teds.rs:424` — `unwrap_or(0.0)` on metric `Option` silently zero-scores errors, corrupting aggregates.
**Change:** Metrics return `Result<f64>`. Harness catches errors, skips the row, logs the failure, and excludes the row from aggregates (does not zero-include it). Delete all `unwrap_or(0.0)` fallbacks.
**Acceptance:** A row that fails a metric is visibly skipped in the harness report and does not appear in denominator counts.

### P2-11. Fix deadline/timeout consistency between client and server
**Motivation:** `crates/hs-scribe/src/config.rs` — no validation that `convert_deadline_secs` ≥ `timeout_policy.ceiling_secs`. Client can request a longer deadline than server will honor; large books get killed mid-convert.
**Change:** Validate at config load: if `ceiling_secs > convert_deadline_secs`, fail to start with a clear error. Single source of truth; no per-request override on top of a conflicting config.
**Acceptance:** Incompatible config values refuse to boot.

### P2-12. Validate NATS event key shape before acting
**Motivation:** `crates/hs-distill/src/event_watch.rs:191-203` — a malformed `scribe.completed` event currently term()s silently (line 200). `event.key` isn't validated before reconcile acts on it.
**Change:** Strict deserialization + key shape validation at the top of the handler. Invalid events are logged + NAK'd (or sent to a dead-letter subject), never silently dropped.
**Acceptance:** Injecting a malformed event produces a logged error and a NAK.

### P2-13. Replace manual YAML scanning with serde
**Motivation:** `hs-common/src/lib.rs:20-48, 91-116` — hand-parses `project_dir:` and `log_dir:` line-by-line. Malformed config silently falls through to defaults.
**Change:** Use `serde_yaml_ng` (already vendored). One config struct, one `?`-propagated load. No line-by-line parse.
**Acceptance:** Malformed YAML in these two fields fails the load with a `serde` error.

### P2-14. Fix `pyke ort` CUDA-libs discovery to fail loudly
**Motivation:** `crates/hs/src/distill_cmd.rs:109-126` — `find_ort_cuda_libs` walks the cache, silently returns `None`. Related memory: "ort pyke CUDA-bundle trap — pyke cache can ship a CUDA-12-only bundle on a CUDA-13 host."
**Change:** If discovery fails, error with a clear message pointing at the remediation (`rm -rf ~/.cache/ort.pyke.io/dfbin/<hash>`). Optionally: check the found `.so` with `ldd` against the host CUDA version and fail if mismatched.
**Acceptance:** A broken pyke cache produces a directly actionable error referencing the cache path.

### P2-15. Stamp `downloaded_at` at bulk-import row synthesis (F6)
**Motivation:** `catalog_repair.flag_drift.would_backfill_downloaded_at: 3350` — Anna's Archive bulk-import items have catalog rows but no `downloaded_at` stamp because the `disk_no_catalog` repair direction synthesized rows without a real download timestamp. Stage-flag introspection is broken for ~98% of corpus. The detection is correct (`hs-common/src/status.rs:618-684`); the fix is at row creation. ONE PATH: row-creation always populates the field; "we don't know" is not an acceptable state.
**Scope:** `hs-common/src/catalog.rs` (row-synthesis call site for the repair direction) and/or `crates/hs/src/scribe_inbox.rs` (bulk-import path).
**Change:** At row synthesis, stamp `downloaded_at` to the disk-file mtime. Do not introduce a separate `imported_at` field — single source of truth on the existing field.
**Acceptance:** After a one-shot `catalog_repair --apply`, `flag_drift.would_backfill_downloaded_at` returns 0. Going forward, no synthesized row leaves the field null.

---

## P3 — DRY and readability refactors

### P3-1. Extract HTTP OCR backend
**Motivation:** `crates/hs-scribe/src/ocr/{cloud.rs, openai_compatible.rs, ollama.rs}` share client construction, base64 encoding, request/parse/error-convert scaffolding. Three near-identical implementations.
**Change:** One `HttpOcrBackend` trait + one helper that handles the common pipeline. Each provider defines only its provider-specific fields (endpoint, auth, request body shape, response struct). Delete the duplicated code — no keeping the old paths.
**Acceptance:** The three providers share the HTTP pipeline; no duplicated client/base64/JSON boilerplate.

### P3-2. Unify 429-retry in paper providers
**Motivation:** `paper/src/providers/response.rs` has two near-identical 429-retry-with-backoff implementations (shared helper + arXiv inlined).
**Change:** Single retry helper. Delete the arXiv inlined version.
**Acceptance:** `rg 'Retry-After|429' paper/src/providers` shows the retry logic only in the helper.

### P3-3. Extract `CommandContext` in the `hs` CLI
**Motivation:** Every `hs <cmd>` repeats "load config → resolve endpoint → dispatch → render." Visible duplication across `serve_cmd`, `scribe_cmd`, `distill_cmd`, `pipeline_cmd`, `cloud_cmd`, `mcp_cmd`.
**Change:** One `CommandContext` built in `main.rs`, handed to every subcommand. Subcommands consume it and do not re-load config or re-resolve endpoints.
**Acceptance:** Subcommand modules contain no `load_config` call; all state comes from the context.

### P3-4. Consolidate paper downloader error logging
**Motivation:** `paper/src/providers/downloader.rs:255-285` — five silent `.ok()` calls hide per-provider failure reasons; operators can't diagnose which resolver broke.
**Change:** One `try_resolve(provider, ...) -> Result<Url, ResolverError>`; the resolver chain collects every error and logs them at the end if all fail. No silent `.ok()` discards.
**Acceptance:** A failing DOI download logs every attempted provider and the specific error.

### P3-5. Simplify `fintabnet` table index parse
**Motivation:** `crates/hs-scribe/src/eval/datasets/fintabnet.rs:86` — `unwrap_or(0)` on malformed table index defaults to table 0, corrupting scores.
**Change:** Return `Err`; skip the row; log. No default index.
**Acceptance:** Malformed rows are visibly skipped in the report.

### P3-6. Channel-send backpressure on server progress stream
**Motivation:** `crates/hs-scribe/src/server.rs:297, 309, 317, 331` — `let _ = tx.send()` drops progress silently when the client disconnects.
**Change:** On send failure, log at `debug!` and cancel the remaining work. No "send and forget" on a broken channel.
**Acceptance:** A disconnected client triggers a visible cancellation log and doesn't leak in-flight work.

### P3-7. Fix `fmt_bytes` unit labeling consistency
**Motivation:** `hs-common` uses decimal (1_000) thresholds — consistent internally but unlabeled. Users of `hs status` are likely to expect binary.
**Change:** Pick one (decimal is the more common convention in observability output) and label the unit explicitly where shown. No "smart" fallback between the two.
**Acceptance:** Every byte display in `hs status` shows the unit and matches a single documented convention.

### P3-8. Remove `unreachable!()` after exhaustive match
**Motivation:** `paper/src/models.rs:107` — Rust already enforces exhaustiveness; the arm is dead code that could mislead future readers.
**Change:** Delete the arm.
**Acceptance:** `rg 'unreachable!\(\)' paper` returns zero matches.

---

## P3 — Test coverage gaps

### P3-9. Add concurrent-writer test for catalog
**Motivation:** P1-12 fixes atomicity but needs a regression test.
**Scope:** `hs-common/src/catalog.rs` tests.
**Change:** Property test: N tasks concurrently call different `update_*_catalog` for the same stem; assert all sections are preserved at the end.
**Acceptance:** Test passes after P1-12; removing the atomicity fix makes it fail.

### P3-10. Add race test for auth token refresh
**Motivation:** P1-4 fixes the refresh storm; needs a regression test.
**Scope:** `hs-common/src/auth/client.rs` tests.
**Change:** Mock refresh endpoint with a delay; fire 100 concurrent `get_access_token()` calls; assert exactly one refresh is issued.
**Acceptance:** Test passes after P1-4.

### P3-11. Add CUDA-feature-guard test
**Motivation:** P0-8 mandates the distill binary refuses to start without `--features cuda`.
**Scope:** `crates/hs-distill/src/server_main.rs`.
**Change:** Add a `#[cfg(not(feature = "cuda"))]` compile-error!("distill server requires --features cuda") at the top of the server binary.
**Acceptance:** `cargo build -p hs-distill --bin hs-distill-server` without `--features cuda,server` fails at compile.

---

## Open questions (carried from rc.308 self-test, 2026-04-26)

Not actionable as backlog items yet; need an operator answer or a follow-up read first:

1. Was the F1 round-trip routed to `.110` or `.111`? Add routed-instance URL to `scribe_convert` response (small observability win; relates to P1-16).
2. Are the F1 doc's 35 chunks all from page 1, or did pages 2–21 also embed? Add chunks-per-page to indexed payload.
3. `git log v0.0.1-rc.307..v0.0.1-rc.308 -- crates/hs-scribe/src/` — does a guardrail commit align with the QC removal at `event_watch.rs:171-176`? Informs P0-12 step 1.
4. Inbox watcher: `last_sweep_found: 3, relocated: 0` — what blocks relocation for those 3 files (permissions, name collision, lock)? Read the relocation logic before raising the threshold from 3.
5. Corpus-wide F3 blast radius: how many docs have markdown < 500 B/page? Run a `markdown_list` paginated screen before P0-13 lands so the cleanup campaign is correctly scoped.

## Operational TODOs (one-shots, not code)

1. **F7 backfill.** Run `catalog_repair --apply` once to backfill the `md_path_drift` rows (DOI-stems with empty `markdown_path` and missing `conversion` stamp; exact overlap with `stuck_convert`). Atomicity confirmed at `hs-common/src/catalog.rs:463-487`; no code change required. 2026-05-02 self-test count: 20 md_path_drift, 21 stuck_convert (15 PDF + 6 HTML), 3367 missing `downloaded_at`. Blocked on the missing `hs catalog repair --apply` CLI (and **P2-15** for `downloaded_at` at synthesis time).
2. **F8 backfill.** After **P1-15** lands, run `hs catalog purge` for the 27 orphan catalog rows surfaced by `catalog_repair.catalog_no_source` on 2026-05-02.
3. **Qdrant outage root cause: rootless podman dies with user-systemd at logout (2026-05-02 — RESOLVED).** Container `home-still_qdrant_1` was killed twice in this session — both times the trigger was `Stopping User Manager for UID 1000` in the system journal (verified at 2026-05-02 11:29:02 CDT). With `Linger=no` for `ladvien`, `systemd --user` exits at the last SSH logout and takes every rootless container with it. `restart: unless-stopped` and `restart: always` are both no-ops in this scenario because there's no podman daemon left to honor them. **Fixed in-session via `loginctl enable-linger ladvien`** (verified `Linger=yes`). The previous BACKLOG hypothesis ("podman-compose@home-still.service stopped it") was wrong — that unit name was a podman label artifact, not a real systemd unit. **Follow-up TODO:** every other rootless service on `big` has the same exposure; audit `home-still-scribe-inbox.service`, `hs-scribe-watch-events.service`, `hs-distill-watch-events.service`, and the timers — confirm they recover correctly across a logout/login cycle now that linger is on, OR convert them to system-level units if any don't.

4. **`hs status` MCP routing depends on cloud OAuth even on the gateway-host itself (2026-05-02 — PARTIALLY FIXED).** `crates/hs/src/mcp_client.rs:from_default_creds` hardcoded `gateway_url + /mcp` so `hs status` on `big` (which hosts the local MCP at `localhost:7445`) round-trips through `cloud.lolzlab.com` and depends on a 7-day refresh token. Token expiry → 401 → silent-fallback dashboard (P0-16). **Mitigation in-session:** added `HS_MCP_URL` env-var bypass in `mcp_client.rs:from_default_creds`; built and installed as `~/.local/bin/hs.rc314-local-mcp` with `~/.local/bin/hs` symlink updated. Operators on the gateway host should `set -gx HS_MCP_URL http://localhost:7445/mcp` in fish config (or `export HS_MCP_URL=...` in bash). **Real fix still owed:** make this a `mcp.url` field in `~/.home-still/config.yaml` instead of an env var — that's the canonical config-side surface the project uses for everything else (storage, scribe, distill, qdrant). Track jointly with **P0-16**.

---

## rc.341 deploy follow-ups (2026-06-17) — ollama autotuner retired

rc.341 removed the orphaned `OLLAMA_NUM_PARALLEL` autotuner, the native-ollama startup glue, and fixed the vestigial "Ollama URL" startup log (it printed `ollama_url` regardless of backend; now prints the active "Backend URL"). The scribe GLM backend is `HS_SCRIBE_BACKEND=OpenAi` → llama-server `:8080`; ollama `:11434` is unused by home-still. See memory `project_scribe_glm_is_llama_server`.

1. **`vlm_concurrency` (12) now oversubscribes llama-server `--parallel 8`.** rc.341 deleted `resolve_effective_vlm_concurrency`, which used to clamp the scribe server's page-parallelism semaphore down to a detected `OLLAMA_NUM_PARALLEL`. That clamp only ever applied to the (unused) ollama path, so removing it is correct — BUT the GLM scribe (`:7433`) now sends up to `config.vlm_concurrency=12` concurrent page requests to `llama-server-glm-ocr` which runs `--parallel 8`, so 4 queue. GLM is escalation-only (rare), so low blast radius, but consider setting the GLM scribe's `vlm_concurrency` to match `--parallel` (8) in config, OR raise llama-server `--parallel`. Tuning only — no rebuild.
2. **Leftover disabled unit file on `big`.** `/etc/systemd/system/hs-serve-scribe-autotune.service` still exists (now `disabled`, inert — rc.341 no longer generates it). Harmless, but `sudo rm` it + `daemon-reload` for cleanliness whenever convenient.
3. **GLM escalations on repetition-loop papers struggle.** Papers that escalate olmocr→glm_ocr via `reason=vlm_repetition_loop` (e.g. `10.3389_fphys.2021.677581`) also trip GLM's 4-gram-cycle repetition detector; some exhaust the chain (`scribe convert failed`) and stay unconverted. This is the same root theme as **P0-12** (decode-time repetition prevention) — both backends loop on the same pathological PDFs. Track under P0-12, not separately.

---

## Code-review follow-ups (2026-06-23) — `feat/distill-search-include-text` branch

Verified findings from a max-effort multi-agent review of the branch diff (145 changed files) vs `main`. **Coverage is partial:** the review's synthesis pass and several verifiers/the codebase-wide sweep hit a session rate-limit, ~24 findings were kept but the report capped at 15, and `openai_compatible.rs` / `reconcile.rs` / `status.rs` were never verified. **Re-run the review for the full set.** Security (CR-1) and documentation/privacy (CR-8) items are normally excluded from this file per the header, but are included here at operator request — both are CLAUDE.md non-negotiables.

> **RESOLVED 2026-06-23 (CR-1 … CR-8).** All eight major/medium findings fixed on
> this branch with regression tests; `cargo fmt`/`clippy -D warnings`/`test` all green.
> - **CR-1** — deleted `is_rfc1918_source` + `lan_zone_claims`; every proxied request now
>   authenticates via the signed-token path (ONE PATH). **Deploy note:** LAN hosts must
>   present a valid bearer token — confirm each is enrolled before rollout.
> - **CR-2** — `citations(sort="citations")` paginates the full citing set (capped at
>   `MAX_CITATION_SORT_FETCH=10_000`) before ranking; test
>   `citations_sort_by_citations_ranks_globally_across_pages`.
>   **rc.344/rc.345 follow-up:** rc.343's deeper pagination 400'd on high-citation
>   papers (SS rejects when `offset + limit >= 10000`; filtered null edges let
>   `offset` outrun `entries`). rc.344's first guard was off-by-one (permitted the
>   failing offset=9000); rc.345 corrected it to break when `offset + PAGE_SIZE >=
>   10000` (verified empirically: offset 8999 ok, 9000 → 400). Tests:
>   `citations_sort_stops_at_ss_offset_ceiling_without_400` (mock) +
>   `citations_sort_live_high_citation_paper_no_400` (live, `#[ignore]`).
> - **CR-3** — `relevance_score` is pure again; title-presence floor moved to
>   `passes_citation_title_floor`, applied only on the citation-sort filter in `search.rs`;
>   test `abstract_match_not_demoted_in_default_ranking`.
> - **CR-4** — `convert_one` retry loop now wraps only server acquisition; convert is
>   single-attempt (comment matches behavior).
> - **CR-5** — `reconstruct_abstract` bounds `max_pos` at `MAX_ABSTRACT_POSITION=100_000`,
>   skips+logs malformed records; test `skips_abstract_with_implausible_position`.
> - **CR-6** — title length gate counts chars; tests `accepts_long_cjk_title_within_char_limit`,
>   `rejects_over_long_title_by_char_count`.
> - **CR-7** — `coalesce_abstract` gates on `chars().count()` across all paths; test
>   `coalesce_gates_on_chars_not_bytes`.
> - **CR-8** — real LAN IPs replaced with `example.local` / RFC 5737 `192.0.2.x`
>   placeholders across docs, rustdoc, and fixtures (incl. `hs-gateway/src/config.rs`,
>   `hs/src/server_cmd.rs`, repro tests — wider than originally listed).
> - **CR-9** (P3 cleanups) and the older codebase-wide P1 sweeps remain **open**.

### P0 (security) — CR-1. Gateway auth bypass via trusted RFC1918 source IP
**Motivation:** `crates/hs-gateway/src/proxy.rs:47` — `is_rfc1918_source` grants wildcard (`*`) scope with synthetic claims and **zero token check** to any connection whose peer IP is in 10/172.16/192.168. Any LAN host (or a container/NAT path presenting a private source IP) reaches every gateway-proxied backend with full scope and no bearer token — a request that previously required a signed token now bypasses auth entirely.
**Scope:** `crates/hs-gateway/src/proxy.rs:47` (+ the claims-synthesis path it feeds).
**Change:** Remove source-IP-based authorization. Every proxied request authenticates via the same signed-token path regardless of source network. No "trusted LAN" branch — ONE PATH.
**Acceptance:** A request from an RFC1918 source with no/invalid bearer token is rejected with 401; a valid token from any source succeeds. (Note: the `proxy.rs` re-verify itself was rate-limited — re-confirm on rerun.)

### P1 (correctness) — CR-2. `paper_citations(sort="citations")` ranks only the first 1000 edges
**Motivation:** `paper/src/providers/semantic_scholar.rs:376` — the fetch loop breaks once `entries.len() >= effective_limit`, which page 1 (`PAGE_SIZE=1000`) always satisfies for default `limit=100`. So it sorts/truncates only the first 1000 fetched edges (in SS default order), not the global citation ranking. For any paper with >1000 citations the MCP tool returns the most-cited 100 *among an arbitrary first 1000* — wrong "top citing papers."
**Scope:** `paper/src/providers/semantic_scholar.rs:376` (citations pagination + sort).
**Change:** When `sort="citations"`, fetch all citing edges (paginate to completion, within a sane cap) before sorting/truncating — don't early-break on `effective_limit` while a sort is requested.
**Acceptance:** `paper_citations` on a >1000-citation DOI with `sort="citations"` returns the globally most-cited N, stable across repeated calls.

### P1 (correctness) — CR-3. Title-presence floor leaks into relevance-sorted search
**Motivation:** `paper/src/aggregation/relevance.rs:76` — the new `TITLE_PRESENCE_FLOOR` cap (comment: "only for sort=citations") is applied unconditionally inside `relevance_score`, which also feeds the relevance-sorted ranking (`ranking.rs:48`, `score = 0.4*rrf + 0.35*rel + …`). A normal `paper_search` whose best match has query terms in the abstract but <50% in the title gets hard-capped at 0.299, demoting on-topic results below weaker title-keyword matches.
**Scope:** `paper/src/aggregation/relevance.rs:76`; caller split in `paper/src/aggregation/ranking.rs:48`.
**Change:** Apply the cap only on the citation-sort path, not inside the shared `relevance_score` used by the default ranking. Thread the intent through (or compute the cap in the citation-sort filter), so relevance ordering is unchanged.
**Acceptance:** A relevance search where the top abstract-match lacks title terms ranks it where it ranked pre-branch; citation-sort still applies the floor.

### P1 (correctness) — CR-4. `convert_one` retry loop never retries a failed conversion
**Motivation:** `crates/hs/src/scribe_pool.rs:33` — the `for attempt in 0..3` loop forwards to `convert_with_progress(...).await?`; `?` returns immediately, and `pdf_bytes`/`on_progress` are moved on the first iteration. So it only ever retries `pick_server()` (pool-saturation) failures, never a failed conversion, despite the loop/comment advertising retry. A transient backend 500 or dropped mid-convert stream fails the whole PDF on attempt 1.
**Scope:** `crates/hs/src/scribe_pool.rs:33`.
**Change:** Either retry the convert step on transient errors (clone/Arc the bytes so they survive iterations; classify transient vs permanent) OR delete the misleading `0..3` loop and document single-attempt semantics. Pick one — no half-loop that lies about its behavior.
**Acceptance:** A simulated transient convert failure is retried up to the advertised count; a permanent failure returns immediately. If retry is dropped instead, the comment and loop are gone.

### P1 (robustness) — CR-5. Unbounded allocation on one corrupt OpenAlex position
**Motivation:** `crates/openalex-ingest/src/parser.rs:42` — `reconstruct_abstract` does `vec![""; max_pos + 1]` where `max_pos` is an unbounded `u32` straight from snapshot `abstract_inverted_index` JSON. One malformed record (position near `u32::MAX` → ~68 GB) OOM-kills `hs openalex load-works` mid-partition, forcing a from-scratch partition re-run — the length-driven blowup the ingest hardening targets.
**Scope:** `crates/openalex-ingest/src/parser.rs:42`.
**Change:** Bound `max_pos` before allocating (reject/skip the abstract when it exceeds a sane token ceiling). Fail the row loudly (skip + log), not the whole partition.
**Acceptance:** A synthetic record with a giant position is skipped (logged) and ingest continues; valid abstracts reconstruct unchanged.

### P1 (correctness) — CR-6. Over-long-title check uses byte length, rejecting valid CJK titles
**Motivation:** `personal/src/services/naming.rs:90` — `title.len() > 200` measures bytes, not chars. A ~70-char CJK title (~210 UTF-8 bytes) is rejected with "model returned over-long title" and the document **fails ingest entirely**. The sibling `take_chars` in the same file correctly uses char boundaries.
**Scope:** `personal/src/services/naming.rs:90`.
**Change:** Compare `title.chars().count() > 200` (or reuse the char-boundary helper).
**Acceptance:** A 70-char/210-byte title passes; a genuinely >200-char title is still rejected.

### P2 (data quality) — CR-7. Abstract length gate mixes bytes and chars by script
**Motivation:** `crates/hs-distill/src/abstracts.rs:120` — `coalesce_abstract` gates OpenAlex/catalog candidates on `s.trim().len()` (bytes) against `MIN_ABSTRACT_CHARS`, while `abstract_chars()` reports chars. A short non-Latin abstract passes as bytes while an equally-short Latin one is dropped to `TitleOnly` — `AbstractSource` provenance becomes script-dependent and inconsistent vs the markdown path.
**Scope:** `crates/hs-distill/src/abstracts.rs:120`.
**Change:** Gate on `chars().count()` consistently across all candidate paths.
**Acceptance:** Latin and CJK abstracts of equal *character* length are accepted/rejected identically; `AbstractSource` stamping is consistent.

### Doc/privacy — CR-8. Real LAN IPs committed in docs and source
**Motivation:** This branch adds real addresses/hostnames, violating the CLAUDE.md privacy non-negotiable (use `<host>`/`example.local`): `docs/deployment.md:890-891` (`192.168.1.110 # big`, `192.168.1.111 # big_mac`) and `crates/hs-scribe/src/config.rs:216-219` rustdoc + test fixtures (`:484-527`, including bmb `192.168.1.233`). The config.rs ones also render in `cargo doc`. Confirmed newly introduced by this branch (surrounding IPs pre-existing).
**Scope:** `docs/deployment.md:890`; `crates/hs-scribe/src/config.rs:216`, `:484-527`.
**Change:** Replace with `http://<host>:7433` / `example.local` placeholders in docs, rustdoc, and fixtures.
**Acceptance:** `git grep -E '192\.168\.1\.(110|111|233)' -- docs crates` returns nothing newly added by the branch.

### P3 (maintainability / efficiency) — CR-9. Lower-priority cleanups
- **`crates/hs-scribe/src/pipeline/processor.rs:745`** — the ~120-line per-region pipeline (stream-render → `spawn_blocking` stage1 → `JoinSet` stage2 → sort/join) is duplicated nearly verbatim between `process_pdf_with_progress` and `process_pdf`, differing only in progress callbacks. This is the rc.304→rc.305 "missed sibling call site" trap (the rc.341 OOM-streaming fix had to be applied twice). Extract one shared driver; `process_pdf` calls it with a no-op progress fn.
- **`crates/hs-scribe/src/event_watch.rs:313`** (and `:463` for HTML) — `(*source.bytes).clone()` copies the full PDF/HTML out of the `Arc` per document. Have `convert_with_progress` accept `Arc<Vec<u8>>`/`Bytes` (reqwest multipart takes `Bytes` without copy).
- **`personal/src/services/ingest.rs:44`** — `converters::convert(cfg, format, bytes.clone(), …)` holds two full file copies across the convert+LLM-naming window (`bytes` reused at `:60`). Change `converters::convert` to take `&[u8]`.

---

## Robustness audit 2026-10-01

Findings from two read-only audits run before the next round of major changes: (1) `hs-scribe` / `hs-distill` / `hs-common`, (2) `hs` / `hs-gateway` / `hs-mcp` / `openalex-ingest` / `paper` / CI. Items already open elsewhere in this file were excluded; findings reported by both audits are merged into one entry with every location cited. Security items are included at operator request (same exception as CR-1 above). Hostnames, domains and usernames are placeholders per the CLAUDE.md privacy rule.

Conventions: IDs `RA-n` are sequential across the whole section. Severity is the audit's, except that any crash/process-kill reachable from untrusted input is raised to P0 (release builds set `panic = "abort"`, RA-5, so one panic kills the whole daemon). `(unverified)` = the audit inferred the behavior and did not observe it at runtime; reproduce before fixing. No code changes were made by the audit; fix per item. Every fix follows the Working rules above: ONE PATH, delete fallbacks, no shims.

> **RESOLVED 2026-10-01 (RA-1 … RA-117).** Fixed on branch `robustness/ra`, tip `ef207f88af84`
> (ten workstreams plus review-driven rounds). Closure audit (independent re-check of every
> item at the tip: code location, regression test, original-failure-pattern grep) is in
> `docs/robustness-audit-2026-10-01-closure.md`. Counts: **84 fixed, 26 fixed-with-residual, 4 needs-decision, 3 moot** (0 open, 0 partial). Each bullet
> below carries a status suffix.
> - **fixed (84):** RA-1, RA-3…4, RA-9…10, RA-13, RA-15…17, RA-20…23, RA-25, RA-27…31, RA-33…34, RA-37…38, RA-42…48, RA-50…55, RA-57…60, RA-62…67, RA-69…72, RA-75, RA-77…81, RA-83…85, RA-87…94, RA-96…101, RA-103…107, RA-109…112, RA-115…116
> - **fixed-with-residual (26):** RA-2, RA-5…8, RA-11…12, RA-14, RA-18, RA-24, RA-26, RA-32, RA-35, RA-39…41, RA-49, RA-56, RA-73…74, RA-82, RA-86, RA-95, RA-102, RA-108, RA-114: closed in code, an operational step or accepted limit remains (see the suffix and `### Operational follow-ups`)
> - **needs-decision (4):** RA-36, RA-61, RA-113, RA-117: owner decision outstanding (RA-OPS-D1…D9)
> - **moot (3):** RA-19, RA-68, RA-76: obsoleted by deleting the gateway dynamic registry (`cloud.gateway.routes` is the one backend resolver)
>
> **Status:** the release-binary integration run (gates a-i, five release binaries, verify-binaries pass/fail checks, hostile-input and token/config smoke) PASSED at tip `ef207f88af84`. Three adversarial review rounds found and drove fixes for all P0/P1 issues; the round-4 fixes (R1–R6) were gated and smoke-tested but NOT adversarially reviewed. Nothing involving live NATS, GPU distill, real VLM, the Legacy converter, macOS/Windows/aarch64, docker, real S3/Qdrant or the full OAuth flow could be tested (see the docs file). **Deployment prerequisites** (token provisioning, re-enrollment, `--gateway-url`, libpdfium, config that is now required/validated, rollout order) are in `docs/deployment.md` § "Upgrade prerequisites (robustness release)"; the remaining operator steps are listed under `### Operational follow-ups (not code)` and new observed-not-fixed findings under `### Observed, not fixed (new findings)` at the end of this section.

### P0

Fix first, in this order: RA-1/RA-2 (gateway auth), RA-3 (distill file read), RA-4/RA-5 (shard panic + abort), RA-6/RA-7 (silent config + Noop bus), RA-8 (SeenSet row loss), RA-9 (reindex purge ordering). RA-10 … RA-15 are crash/OOM paths raised to P0 by the untrusted-input rule.

- **RA-1** `crates/hs-gateway/src/enrollment.rs:216` — admin-invite "localhost-only" gate tests the peer IP with `is_loopback()` and `unwrap_or(true)`. cloudflared connects from loopback (docs/deployment.md lists 7440 as "cloudflared (loopback)"), so every internet request looks local: anyone can `POST /cloud/admin/invite` with arbitrary scopes (even `["*"]`, `token.rs:32`) and enroll. Same trust-by-source-IP class as CR-1. Fix: require a separate admin credential/scope (or a Unix socket), reject requests carrying `cf-*` / `x-forwarded-for`, whitelist grantable scopes, delete the `unwrap_or(true)`. **[fixed]**
- **RA-2** `crates/hs-gateway/src/oauth.rs:161-197` — `/authorize` never checks `redirect_uri` against `oauth_clients` (store written at `:450`, never read) and never enforces `code_challenge_method == S256`; the auth code is redirected to any attacker URL once a user enters their own code. `Redirect::to` also panics on `\n` in `state`/`redirect_uri` (process abort under RA-5). Fix: exact-match a registered `redirect_uri`, URL-encode query params, require S256. **[fixed — residual: open dynamic client registration (redirect-host allow-list not built)]**
- **RA-3** `crates/hs-distill/src/pipeline.rs:80` (via `server.rs` `/distill`, `/distill/stream`) — when `content` is omitted the server does `fs::read_to_string(req.path)` on any client-supplied path, and the text is retrievable through `/search` `chunk_text`. The server binds all interfaces (`server_main.rs:15`) with no auth: unauthenticated arbitrary file read. `DistillClient` even documents that the server "never reads it from disk". Fix: make `content` required and delete the disk read. **[fixed]**
- **RA-4** `hs-common/src/lib.rs:64,71` — `sharded_key` slices `&stem[..stem.len().min(2)]`, which panics when byte 2 falls inside a multi-byte char (`Müller.pdf`, `Año.pdf`, `Cómo.pdf`). Reached from `crates/hs/src/scribe_inbox.rs:138` (user-dropped filenames; the daemon crash-loops on the poison file, which is never removed), `crates/hs-mcp/src/main.rs:1785,1790,1795,1921` (any MCP client stem), `crates/hs/src/migrate_cmd.rs:67` (`hs migrate sharding`), `paper/src/providers/downloader.rs` `build_key`, and scribe `event_watch.rs:109,211` (event-derived stems; a panicking handler leaves the event un-acked until the 7200 s `ack_wait`). Fix: take the prefix with `chars().take(2)` / `char_indices().nth(2)` (identical for ASCII), reject empty/`.`/`..` stems at every boundary, add a non-ASCII test. **[fixed]**
- **RA-5** `Cargo.toml:42` — `[profile.release] panic = "abort"` with no `CatchPanicLayer`: any panic on untrusted input (RA-2, RA-4, RA-10, RA-13, RA-14 …) kills the whole gateway / MCP server / inbox daemon instead of one request. Fix: drop `panic = "abort"` for the server binaries (or add `CatchPanicLayer`/`catch_unwind` at every request and event-handler boundary) AND fix the individual panics. **[fixed — residual: unwind+catch layers; rmcp tool-call/background-task panics and fault-exit wiring untested]**
- **RA-6** config loaders swallow every error and fall back to defaults. `crates/hs-scribe/src/config.rs:456,462,467` and `crates/hs-distill/src/config.rs:215,221` return `Ok(default)` on a malformed `scribe:`/`storage:`/`events:` section (localhost server, LocalFs, Noop bus); both `server_main.rs` (scribe `:44-47`, distill `:45-48`) start on `Default` when `load()` fails; `paper/src/config.rs:92,97` `unwrap_or_default()`s malformed `storage:`/`events:` (downloads land on local default / NoOp bus); `crates/hs-mcp/src/main.rs:559-565,579-580,603-609` does `DistillClientConfig/ScribeConfig::load().unwrap_or_default()` and degrades an event-bus init failure to `NoOpBus`, and carries dead `legacy_*_dir` fields + "TODO remove legacy"; the CLI replaces config errors with `http://localhost:743x` (inconsistent ports 7432/7433/7434/7435) in `scribe_cmd.rs:10,40-42,387,882`, `distill_cmd.rs:13,36-38,425,898,1483`, `pipeline_cmd.rs:18,321,670,940`, and drives filesystem-moving/destructive commands from `Config::load().unwrap_or_default()` default dirs in `migrate_cmd.rs:12-13`, `scribe_cmd.rs:979`, `serve_cmd.rs:179`, `distill_cmd.rs:452,526,748,925`, `restart_cmd.rs:663`, plus `upgrade_cmd.rs:389,472`. A YAML typo therefore makes watchers consume nothing and publish into the void while reporting success. Fix: propagate every load/extract error, delete all `Default`/`unwrap_or_default` fallbacks, fail when `servers` is unset/invalid, delete the legacy fields. **[fixed — residual: some config readers/sections still outside ConfigFile (unknown keys ignored in scribe/distill/cloud.gateway; local storage root ignores home.project_dir)]**
- **RA-7** `hs-common/src/event_bus/config.rs:12,68` (+ `event_bus/mod.rs` `NoOpBus`) — `EventsBackend` defaults to `Noop`; `NoOpBus::publish` returns `Ok` and `consume` is `pending()`. With RA-6, any config fault silently blackholes the pipeline. Fix: require an explicit `events.backend`; keep Noop only when explicitly configured or in tests. **[fixed — residual: template config still writes events.backend: noop; nats url userinfo logged]**
- **RA-8** `crates/openalex-ingest/src/duckdb_loader.rs:355,490-501` + `seen_set.rs:122-125` — `seen.insert()` marks IDs before the bulk INSERT; when a file/partition fails the loader logs and `continue`s with the poisoned in-RAM set, later checkpoints persist it, and a re-run treats the failed partition's rows as duplicates and logs `ok` with 0 rows: those works are permanently missing. Fix: stage IDs per file and commit to `SeenSet` only after a successful INSERT; abort the load on the first failure. **[fixed — residual: production seen_set.bin may be poisoned; operator must move aside and re-run load-works]**
- **RA-9** `crates/hs-mcp/src/main.rs:2351-2372` — `distill_reindex` calls `delete_doc` BEFORE reading the catalog / verifying the markdown, and `exists().unwrap_or(false)` turns an S3 blip into "missing": vectors are deleted, nothing is re-indexed, the catalog embedding stamp goes stale. A destructive MCP tool, contradicting P0-6. Fix: resolve + verify the key first, index the new vectors, then purge old ones — or make it CLI-only per P0-6. **[fixed]**
- **RA-10** `paper/src/providers/downloader.rs:28-29`, `paper/src/providers/arxiv.rs:153` — `doi[..15]` and `&s[..10]` byte slices panic on untrusted DOI / API text (abort in release). Raised P1 → P0. Fix: `str::get(..)` / char-boundary helper; return an error on short input. **[fixed]**
- **RA-11** `paper/src/providers/downloader.rs:347-362` — unbounded in-memory download; `Vec::with_capacity(content_length)` trusts the remote header (allocation abort); no max size; resolver-supplied URLs are fetched with no scheme/host/private-IP checks, and any HTML body is stored as a paper. Raised P1 → P0. Fix: hard size cap, never pre-allocate from the header, validate URLs, reject non-PDF bodies. **[fixed — residual: HTTP(S)_PROXY bypasses the hostname SSRF check]**
- **RA-12** `crates/hs-scribe/src/pipeline/pdf_parser.rs:38-45` — render size is `MediaBox·dpi/72` with no pixel cap; `max_image_dim` downscaling (`processor.rs:949`) runs after the bitmap exists. A hostile MediaBox (e.g. 14400 pt at 200 dpi ≈ 40000² px ≈ 6 GB) OOM-kills the host. Raised P1 → P0. Fix: clamp target dimensions/dpi before rendering. **[fixed — residual: pdfium-backed tests skip silently without libpdfium (no HS_REQUIRE_PDFIUM)]**
- **RA-13** `crates/hs-distill/src/metadata.rs:85` — `&text_sample[..text_sample.len().min(2000)]` panics on a non-char boundary when `llm_metadata: true` (document text is untrusted; gated by that flag). Same file: no Ollama timeout, an invalid port silently becomes 11434 (`:50-52`), and a JSON parse failure returns `Ok(empty)` that overwrites existing meta (`:109-112`, `pipeline.rs:160-170`). Raised P1 → P0. Fix: floor to a char boundary, add a timeout, propagate errors. **[fixed]**
- **RA-14** `crates/hs-scribe/src/server.rs:224-231` / `crates/hs-scribe/src/event_watch.rs:157-164` — `.ok().flatten()` on the page-count `spawn_blocking` hides lopdf panics (full parse of up to 256 MB untrusted PDFs) as "unknown pages" → fallback timeout; under `panic = "abort"` (RA-5) such a panic would instead kill the process (unverified: no malformed PDF was fed to lopdf). Raised P2 → P0. Fix: bound/validate before parsing, log and classify the failure instead of flattening it. **[fixed — residual: objstm decompression bomb (MemoryMax needed), un-killable wedged pdfium call, libpdfium now required on every scribe host]**
- **RA-15** `crates/hs-scribe/src/epub.rs:16-31` — unbounded zip decompression with no entry-count or expanded-size cap: a zip-bomb EPUB OOM-kills the converter (split from the P2 clone/blocking item, RA-95). Raised P2 → P0. Fix: cap entries and total expanded bytes, error when exceeded. **[fixed]**

### P1

- **RA-16** `crates/hs-gateway/src/oauth.rs:132-137` — GET `/authorize` interpolates `client_id` / `redirect_uri` / `state` / `code_challenge` / `scope` unescaped into `value="…"` attributes: reflected XSS on the auth origin. Fix: HTML-escape every interpolated value (or render with askama/maud). **[fixed]**
- **RA-17** `crates/hs-gateway/src/oauth.rs:322` — the OAuth path always mints `scribe,distill,mcp`; `enrollment.scopes` is discarded (`:168-171`). Fix: carry the invite scopes into the auth code and the token. **[fixed]**
- **RA-18** `crates/hs-gateway/src/enrollment.rs:152-178`, `oauth.rs:371-394`, `proxy.rs:28` — access and refresh tokens are indistinguishable (`validate_token` has no type claim): a 7-day refresh token works directly on the proxy; any live access token can be exchanged at `/cloud/refresh` for a fresh one indefinitely; no revocation; `key_rotation_days` (`config.rs:29`) and `validate_token_multi` are unused. Fix: add a `typ` claim, reject refresh tokens on the proxy and access tokens at refresh, implement rotation + revocation. **[fixed — residual: OAuth refresh-token rotation/reuse detection not implemented; all old tokens need re-enrollment]**
- **RA-19** `crates/hs-gateway/src/registry.rs:301-337,~100` — any token with scope `scribe|distill|mcp` can register an arbitrary URL (SSRF; hijack of other users' uploaded PDFs through the round-robin proxy); `register()` blind-inserts, overwriting another device's owner; `set_enabled` has no owner check; no per-device cap. Fix: owner check on overwrite/enable, URL allow-list + private-range check, entry cap. **[moot: gateway registry deleted (D1 option B, routes are the one resolver)]**
- **RA-20** `crates/hs-gateway/src/proxy.rs:61-66` — "registry first, then static config fallback" (ONE PATH violation); `resolve_service` uses `starts_with("/mcp")` (matches `/mcpfoo`) and forwards the un-normalized path (the `url` crate normalizes dot segments). Fix: one source of truth, match on path-segment equality, reject `..` / `%2e`. **[fixed]**
- **RA-21** `crates/hs-gateway/src/proxy.rs:134`, `main.rs:52` — buffers up to 256 MiB per request in RAM with no concurrency limit; the backend client has only `connect_timeout` (no read/total timeout); hop-by-hop and client `X-Forwarded-*` headers are forwarded; backend status via `unwrap_or(StatusCode::OK)` (~`:158`) masks invalid statuses. Fix: stream bodies, add a semaphore + request timeouts, strip hop-by-hop headers, fail on an invalid status. **[fixed]**
- **RA-22** `crates/hs-gateway/src/config.rs:104-121` — signing secret is written then `chmod 0600` (world-readable window); an existing key shorter than 32 B is silently regenerated (invalidates every token); duplicate secret creation in `crates/hs/src/cloud_cmd.rs:44-60`. Fix: `OpenOptions::mode(0o600).create_new(true)`, fail on a short file, one implementation. **[fixed]**
- **RA-23** `crates/hs-gateway/src/main.rs:39-42` — `gateway_url` defaults to `http://{listen}` (plain-http loopback) and is emitted in enrollment responses and OAuth metadata (`oauth.rs:63-82`). Fix: require an explicit https `gateway_url`. **[fixed]**
- **RA-24** `crates/hs-mcp/src/main.rs:3548-3557` + `crates/hs/src/serve_cmd.rs:236` — HTTP MCP binds all interfaces with no auth/Origin/Host check (rmcp 1.3.0 `StreamableHttpServerConfig` has no `allowed_hosts`); the gateway's "no source-IP trust" is bypassed by hitting port 7445 directly, exposing `paper_download` / `scribe_convert` / `distill_*`. Fix: bind loopback only or require a bearer token in hs-mcp. **[fixed — residual: one static shared HS_BACKEND_TOKEN, no TLS/per-service tokens (owner design decision); token must be provisioned before upgrade]**
- **RA-25** `crates/hs-mcp/src/main.rs:3548-3554` + `crates/hs/src/mcp_client.rs:~90-118` — `keep_alive=None` with the default `stateful_mode=true`, and `hs status` never sends DELETE, so sessions accumulate for the process lifetime (unverified: growth inferred from the rmcp session model). Fix: finite idle timeout or `stateful_mode=false`; client DELETE on drop. **[fixed]**
- **RA-26** `crates/hs-distill/src/server.rs:73-78,44-60`, `qdrant.rs:305-311` — no auth on `/collection/reset`, `DELETE /doc/{id}`, `/scrub-interstitials`; `resolve_collection` lets any caller create unlimited Qdrant collections by name (no format/allow-list) and reset any of them; reset is delete-then-create (non-atomic). Fix: loopback/gateway-only bind plus a token, allow-list collection names, make reset atomic or CLI-only. **[fixed — residual: collection reset still drop+recreate (atomic alias reset needs a data migration decision)]**
- **RA-27** `hs-common/src/storage/mod.rs:76-78` — `LocalFsStorage::resolve` is `root.join(key)`: `..` components or absolute keys (`join` replaces the root) escape the root for get/put/delete. Keys come from NATS payloads (scribe/distill `event.key`), MCP `catalog_read` / `markdown_read` / `scribe_convert` stems (put at `crates/hs-mcp/src/main.rs:1921`), and filename stems (`...pdf` → stem `..`). S3's `Path::parse` probably rejects `..`, so impact is LocalFs. Fix: reject absolute / `..` / `\` / NUL keys in `resolve` and return `Err`; validate stems in hs-mcp. **[fixed]**
- **RA-28** `hs-common/src/storage/mod.rs:99` — `LocalFsStorage::put` is a direct `tokio::fs::write`: concurrent readers see truncated markdown/catalog YAML and a crash leaves partials. Fix: write a temp file in the same dir and rename. **[fixed]**
- **RA-29** `hs-common/src/catalog.rs:289-293,309,326,356,378,735` — the sync path API is still alive beside `_via`. `read_catalog_entry` uses `.ok()?` on both read and parse, then `update_*_catalog` / `update_embedding_skip` do `.unwrap_or_default()` + a non-atomic `fs::write`: a corrupt or unreadable row is silently replaced by a near-empty one. P0-10 only fixed `_via`; distill `pipeline.rs:137` still uses the sync reader. Fix: delete the path variants or make them `Result` over the same kernel. **[fixed]**
- **RA-30** `hs-common/src/status.rs:354-368,398-418` — `count_ext_via` / `count_unconverted_stems` return 0 on any `list` error, so drift and doc counts read "all converted" during an S3 outage (false green); `collect_pipeline_counts` cannot return an error. Fix: return `Result`, render "unknown". **[fixed]**
- **RA-31** `crates/hs-mcp/src/main.rs:1803-1831` — `scribe_convert`: `if let Ok(pdf_bytes) = storage.get()` treats any storage error as "not found" and falls through to the local HTML/EPUB converters (the tool description even advertises HTML "fallbacks"). Fix: `head` + `is_not_found`, error otherwise; remove the local-converter path (ONE PATH). **[fixed]**
- **RA-32** `crates/hs-mcp/src/main.rs:971,2151,2241,2377,2489` — a catalog write failure after download only `warn`s yet the tool returns success; `exists().unwrap_or(false)` in index/reconcile/backfill reports false orphans on storage errors (feeds CLI purge). Fix: propagate storage errors. **[fixed — residual: paper_download post-download papers.ingested publish failure only warns (see RA-125)]**
- **RA-33** `crates/openalex-ingest/src/duckdb_loader.rs:665-720` — every `append_row` is `let _ =`; a failed work row still leaves its authorship/topic/ref edges (orphans) and its ID in `SeenSet` (the comment at `:636-640` claims nothing can fail). Fix: propagate and abort. **[fixed]**
- **RA-34** `crates/openalex-ingest/src/duckdb_loader.rs:~447-460` — five INSERTs auto-commit separately (no transaction): a failure after `works` leaves partial data and retries duplicate `work_authorships` (no PK). Fix: wrap in one transaction. **[fixed]**
- **RA-35** `crates/openalex-ingest/src/duckdb_loader.rs:496-501,552-556`, `reader.rs:82-100`, `crates/hs/src/openalex_cmd.rs:92-97` — partition failures are swallowed, `load_works` returns Ok (exit 0), the `_ingest_log` "error" status is never written; parse errors are only counted (no threshold) and the partition is logged `ok`; authors append failures only warn. Fix: fail when failures/parse_errors exceed a tiny threshold, write the error status, exit non-zero. **[fixed — residual: parse-error threshold 100/partition is a decision; authors partitions non-atomic (RA-128)]**
- **RA-36** `crates/hs/src/migrate_cmd.rs:~320-335,77` — `relocate_one`: `if let Ok(Some(..)) = head(tgt)` swallows head errors then overwrites the target; equal size is treated as identical and the source is deleted; `fs::rename` overwrites silently. Fix: propagate head errors, compare hash/etag, `create_new`. **[needs-decision: no put_if_absent (decision: not added); head->put race accepted]**
- **RA-37** `crates/hs/src/pipeline_cmd.rs:386-463,296-306` — `rebuild` drops the Qdrant collection first, then deletes markdown/catalog; collected `errors` still return `Ok(())` (exit 0); `catch-up` same; inventory `unwrap_or_default()` (`:506-508`) shows 0 docs when distill is down. Fix: return `Err` when errors > 0, fail inventory loudly, order deletes after verification. **[fixed]**
- **RA-38** `paper/src/providers/downloader.rs:265-298` — each resolver is `if let Ok(..)`: storage/IO errors are swallowed and reported as "No open-access PDF found"; the chain arXiv → MDPI → Unpaywall → PMC → providers is a multi-path fallback. Fix: distinguish NotFound from infra errors, fail fast on IO. **[fixed]**
- **RA-39** `paper/src/services/download.rs:142,191` vs `paper/src/providers/downloader.rs:258` — two storage-identity schemes (`paper.id` sanitized vs lowercased DOI) → duplicate stems (the 42-pair incident); `sanitize_filename` leaves `..`. Fix: one canonical stem function. **[fixed — residual: id-keyed documents already stored are not renamed]**
- **RA-40** `crates/hs-distill/src/embed/onnx.rs:104` + `config.rs:61` + `chunker.rs:12-17` — `InitOptions::new(BGEM3)` has no `.with_max_length`; fastembed 5.13.2 defaults to 512 tokens (`text_embedding/mod.rs:6`) and truncates in the tokenizer while chunks default to 1000 tokens (~4000 chars): the back half of most chunks is silently absent from its vector. Fix: set max_length ≥ chunk size (bge-m3 allows 8192) or reject `chunk_max_tokens > max_length` at config load. **[fixed — residual: corpus re-embed required (pre-change vectors cover only first 512 tokens)]**
- **RA-41** `crates/hs-distill/src/qdrant.rs:58` — every collection is created with HNSW `m(0)` ("bulk load"); the only HnswConfig/`update_collection` reference in the workspace is that line, so search is brute-force forever and `hnsw_ef(128)` (`:175`) can never apply. Fix: real HNSW params at creation, or an explicit post-load `enable_hnsw` step. **[fixed — residual: existing three collections still m=0 until `hs distill hnsw enable` is run]**
- **RA-42** `crates/hs-distill/src/pipeline.rs:83-120,176-232` — point IDs are `doc_id:chunk_index` with no pre-delete: re-indexing a doc that now yields fewer chunks (reconvert, quality filter) leaves stale tail chunks, and every `Ok(0)` skip path (empty / interstitial / zero chunks, `:85,119,219`) leaves prior chunks searchable while the catalog is stamped skip. Fix: delete by `doc_id` (or `chunk_index ≥ new_total`) around the upsert and on skip. **[fixed]**
- **RA-43** `hs-common/src/html.rs:179` (used by distill `pipeline.rs:112` and `qdrant.rs:401`) — whole-document and per-chunk substring gate on generic phrases ("preparing to download", "just a moment...", "data, including cookies, are used to provide services") with no size gate: any real book containing one is skipped (terminal `embedding_skip`) and `scrub_interstitial_chunks` deletes legitimate chunks. Fix: apply only to short docs/chunks. **[fixed]**
- **RA-44** `crates/hs-distill/src/qdrant.rs:40-43,63-70` — `ensure_collection` returns Ok if the name exists (no vector size/distance/index check); a failure in `create_indexes` after `create_collection` leaves a collection every later start accepts without indexes. Fix: verify config when it exists, create indexes idempotently on every start. **[fixed]**
- **RA-45** `crates/hs-distill/src/embed/onnx.rs:191-198,230,271` — (a) `texts.to_vec()` re-copies all chunk texts (pipeline already clones at `pipeline.rs:231`); (b) a panic inside ORT poisons the slot mutex, so every later embed returns "Model lock poisoned" while `/health` stays 200 (it only checks Qdrant); (c) the idle sweeper takes a std `Mutex` on a tokio worker and blocks while an embed holds it; (d) `step_by(batch_size)` panics for `batch_size: Some(0)`. Fix: report embedder health, exit on poison, sweep via `spawn_blocking`, validate ≥ 1. **[fixed]**
- **RA-46** `crates/hs-distill/src/client.rs:140,212-216` — `index_content_in` has no request timeout and the authed client (`hs-common/src/auth/client.rs:177`) has none either: a stalled server hangs the handler forever holding a concurrency permit while the 2 h `ack_wait` elapses. Fix: bounded per-request timeout. **[fixed]**
- **RA-47** `crates/hs-distill/src/event_watch.rs:253` (scribe twin `crates/hs-scribe/src/event_watch.rs:566`) — `run_subscriber` returns `Ok(())` when the message stream ends (consumer deleted by a competing watcher, broker drop; `nats.rs:201` swallows errors), so the caller exits 0 and consumption silently stops (unverified: systemd `Restart=always` is inferred not to restart on a clean exit, cf. P1-19). Fix: return `Err("event stream ended")`. **[fixed]**
- **RA-48** `crates/hs-scribe/src/converter/olmocr_subprocess.rs:100-102` — only `completed == 0` is an error; `failed > 0` pages are accepted and the partial markdown is stamped success (only logged). Fix: fail/escalate when `failed > 0` or `completed` ≠ source page count. **[fixed]**
- **RA-49** `crates/hs-scribe/src/pipeline/processor.rs:531-545,780-794` — full-page mode turns any VLM or JPEG error into an empty page ("paper continues") while per-region mode is strict (`:1230+`), so a backend outage mid-book yields a gapped "successful" conversion; per-region skipped regions (`:1099`) log "partial" but never reach QC or the catalog. Fix: propagate errors, feed skipped counts into the QC verdict. **[fixed — residual: any skipped region now rejects the conversion (policy accepted; more NAK traffic under VLM instability)]**
- **RA-50** `crates/hs-scribe/src/pipeline/processor.rs:887-905` (+ `crates/hs-scribe/tests/end_to_end_test.rs:8`) — a missing/failed layout ONNX model logs "Falling back to FullPage mode" and the server still comes up with `/health` 200; the processor itself documents full-page VLM as the repetition-loop path (`:225-230`), and the e2e test asserts `Processor::new(default)` is ok. Fix: fail startup in per_region mode, delete the fallback (same for the table pool). **[fixed]**
- **RA-51** `crates/hs-scribe/src/server.rs:195,344` + `server_main.rs:61` + `pipeline/processor.rs:116-139` — in `Olmocr` mode the `/scribe*` path never takes `vlm_sem`, so `/readiness` slots are constant (the pool never sees the host as busy) and nothing server-side caps concurrent `olmocr` CLI subprocesses; `Processor::new` still loads up to `detector_pool_size` (≤ 8) layout + table ONNX sessions on CUDA, unused, eating VRAM the VLM backend needs. Fix: a real converter semaphore reported by `/readiness`; build `Processor` only for Legacy. **[fixed]**
- **RA-52** `crates/hs-scribe/src/ocr/openai_compatible.rs:96-103` (+ `:75`, `sse_buffer.rs:93`) — clean EOF without `[DONE]` returns `Ok(partial)`; `finish_reason: length` (`max_tokens` 8192) and mid-stream `{"error":…}` events are ignored (only `delta.content` is read); an invalid-UTF-8 SSE event is silently dropped: truncated regions are stamped success. Fix: require `[DONE]` or `finish_reason == "stop"`, else `Err`. **[fixed]**
- **RA-53** `crates/hs-scribe/src/ocr/openai_compatible.rs:33`, `ocr/cloud.rs:15` — `reqwest::Client::new()` means no connect/read timeout on the VLM stream (hs-common `http.rs` exists to prevent this); a stalled backend pins a VLM permit until the whole-convert deadline. The cloud backend also uploads private pages to a third party when configured. Fix: `http::client_builder()` with connect and read/idle timeouts. **[fixed]**
- **RA-54** `crates/hs-scribe/src/client.rs:38` — `raw.clamp(floor_secs, ceiling_secs)` panics when `floor_secs > ceiling_secs` (config-controlled, unvalidated, in the dispatch path); `vlm_concurrency=0` / `page_parallel=0` (`pipeline/processor.rs:142,660`) make `Semaphore(0)` hang and `region_parallel=0` makes `buffered(0)` hang (`:306,435,1310`). P2-11 covers only deadline vs ceiling. Fix: validate at load. **[fixed]**
- **RA-55** dead second conversion paths in hs-scribe — `crates/hs-scribe/src/server.rs:142,310-376` `POST /scribe` (no caller; `client.rs:298` says `/scribe/stream` is the ONE path); `Processor::process_pdf` (`pipeline/processor.rs:726-877`, which also lacks the `page_sem` cap so prepared pages pile up at `:853-856`); `crates/hs-scribe/src/watch.rs:12` `watch_directory` (zero callers; writes `.md` beside the PDF with no QC/catalog/magic-byte gate, blocking `mpsc::recv_timeout` in async). Fix: delete all three plus the `pdf-mash` Watch/Convert CLI args if unused (deleting `process_pdf` also resolves CR-9's duplicated-pipeline bullet). **[fixed]**
- **RA-56** `crates/hs-scribe/src/event_watch.rs:449-465` — a failed conversion catalog stamp and a failed `scribe.completed` publish are `warn!` only; the markdown is committed and the event ACKed, so distill never learns of it. Fix: make a publish failure Transient (redelivery lands on `AlreadyConverted`, `:117`; confirm that caller republishes `scribe.completed`). **[fixed — residual: catalog stamp loss after committed conversion only logged at ERROR (catalog_repair reconciles)]**
- **RA-57** `crates/hs-scribe/src/server.rs:224-231,300-306,147` — `/scribe*` buffers up to 256 MiB per request (`field.bytes()` then a `.to_vec()` copy), writes it to a temp file even in Olmocr mode (and again inside `olmocr_subprocess::convert`), and has no cap on concurrent uploads; `while let Ok(Some(..))` converts multipart errors (including the body limit) into "Missing 'pdf' field". Fix: admission semaphore, stream to disk, surface the real error. **[fixed]**
- **RA-58** `hs-common/src/auth/client.rs:159-180` — `build_reqwest_client` bakes one short-lived access token into `default_headers` and sets no overall timeout; `crates/hs/src/distill_cmd.rs:20` and `scribe_cmd.rs:24` hand it to long-running watch daemons, and `DistillClient` sets no per-request timeout (unverified: 401s after the token TTL are inferred and depend on daemon lifetime). Fix: attach the token per request via `get_access_token()`; set a client timeout. **[fixed]**
- **RA-59** `hs-common/src/service/pool.rs:165-167` — `pick_lock` is held across `try_pick_once`, which `join_all`s N `/readiness` probes (5 s timeout each on a sleeping host), so all concurrent pickers serialize behind the slowest dead host. Distinct from P1-23's poll-wait parking. Fix: probe outside the lock (short-TTL snapshot) and lock only the claim. **[fixed]**
- **RA-60** `hs-common/src/gpu.rs:37-42` — `nvidia_smi` is a blocking `std::process::Command::output()` with no timeout, called from async handlers: scribe `server.rs:157` per `/health`, `backend_probe.rs:92` per readiness probe, `server.rs:108` while holding the `backend_state` tokio mutex (`:83`). A wedged driver — the very failure it diagnoses — parks runtime workers. Fix: `spawn_blocking` + timeout, cached for a TTL. **[fixed]**
- **RA-61** `crates/hs/src/upgrade_cmd.rs:262-330` — downloads and installs binaries (hs, hs-gateway, hs-mcp, scribe/distill servers) with no checksum/signature verification although `.github/workflows/release.yaml:249-256` publishes `.sha256` + `checksums-sha256.txt`; `crates/hs/src/mcp_cmd.rs:391-445` is a duplicate download/extract/install path with a duplicate `detect_target` (`:306-328`). Fix: one installer; verify sha256 (ideally a signature) before write; run `<bin> --version` and compare to the tag. **[needs-decision: checksum-only; signature verification/infra is an owner decision]**
- **RA-62** `crates/hs/src/upgrade_cmd.rs:278,83-92,140` — a missing asset returns `Ok(false)`; the Windows target looks for `.tar.gz` while the release ships `.zip`, so `hs upgrade` installs nothing and prints "Upgraded to X". Fix: bail on a missing `hs` asset; implement zip or reject Windows. **[fixed]**
- **RA-63** `crates/hs/src/upgrade_cmd.rs:203-206,232-235,283` — no timeout on any request, the whole archive via `resp.bytes()` in RAM, unbounded `read_to_end` (decompression bomb), fixed tmp name `.hs.upgrade.tmp` never cleaned or locked. Fix: timeouts, size caps, unique tmp + cleanup. **[fixed]**
- **RA-64** `crates/hs/src/upgrade_cmd.rs:438,418-455,463-495` — compose `down` is ignored, pull/up failures only `warn` then `Ok(())`; the health check hard-codes ports 7433/7434 and only warns; never asserts distill `compute_device == cuda`. Fix: fail on non-success, take ports from config, assert CUDA (the `unwrap_or_default()` config loads at `:389,472` are covered by RA-6). **[fixed]**
- **RA-65** `crates/hs/src/restart_cmd.rs:170-172,227-260` — a system-unit discovery failure (`unwrap_or_default`) or an unparsable unit (`?` → skipped) yields "No running services found to restart" while the old code keeps running. Fix: treat discovery/parse failure as an error. **[fixed]**
- **RA-66** `crates/hs/src/restart_cmd.rs:~690-715,635,~648` — compose `restart` non-zero with empty filtered stderr prints "OK"; `count += 1` even on Err; `ComposeCmd::detect()` `None => Ok(0)`; `ensure_index_running()` result ignored; `std::thread::sleep` in async (also `distill_cmd.rs:567,706`). Fix: check exit codes, `tokio::time::sleep`. **[fixed]**
- **RA-67** `crates/hs/src/serve_cmd.rs:575-583,534` — unit written to a predictable `/tmp/<svc>.service` then `sudo cp` (symlink/TOCTOU → root unit); `User=` falls back to a hard-coded developer username. macOS: `serve_cmd.rs:418,662` embeds `secrets.env` values unescaped in a default-umask plist; logs go to `/tmp/hs-*.log`. Fix: `tempfile` 0600 in a private dir or `sudo tee`; require `$USER`; XML-escape + chmod 0600. **[fixed]**
- **RA-68** `crates/hs/src/serve_cmd.rs:236,~939-976,1056,854` — `local_ip_hint()` falls back to loopback, so a remote gateway proxies to itself; registration failure is best-effort `warn`; a heartbeat 404 after a gateway restart (in-memory registry) is only logged and never re-registers; the Drop-time deregister is spawned on a possibly-dead runtime. Fix: fail on no IP, re-register on 404. **[moot: registry/heartbeat code removed with the gateway registry]**
- **RA-69** `crates/hs/src/mcp_client.rs:117-118,152,97,104` — `post()` ignores the HTTP status (`text().unwrap_or_default()`), `isError:true` results are returned as data, the handshake notify is `let _ =`. Fix: check status, surface `isError`. **[fixed]**
- **RA-70** `crates/hs/src/cloud_cmd.rs:80,133-160,233` — invite hard-codes the loopback `:7440` gateway address, enroll accepts `http://` gateways and trusts the server-supplied `gateway_url`, only connect timeouts, prints a fixed "4 hours"; `crates/hs/src/main.rs:262-269` writes `secrets.env` then `let _ = chmod`. Fix: read listen from config, require https, set 0600 at create. **[fixed]**
- **RA-71** `paper/src/providers/resilient.rs:51-72` + `paper/src/resilience/circuit_breaker.rs` — the breaker only calls `is_call_permitted()`, never `on_success` / `on_error` / `call()`, so it can never open; limiter/breaker are rebuilt per `make_provider` call (`paper/src/commands/paper.rs:~403-408`), and hs-mcp `paper_download` rebuilds providers/limiters/storage per call (`crates/hs-mcp/src/main.rs:~870-895`), so MCP calls share no rate-limit/circuit state (Semantic Scholar 429s). Fix: use `cb.call()`; share provider instances across calls. **[fixed]**
- **RA-72** `paper/src/services/search.rs:~96-110,170-182`, `paper/src/providers/response.rs:46-58` — partial provider failure is invisible (only `tracing::warn`); `get_by_doi` maps errors/timeouts to `Ok(None)` ("not found" when every provider is rate-limited); `Retry-After` is honored uncapped (sleep for hours). Fix: return per-provider errors, `Err` when none succeeded, cap the sleep. **[fixed]**
- **RA-73** `crates/hs-distill/build.rs`, `crates/hs-scribe/build.rs` (identical copies), `crates/hs/build.rs`, `crates/hs-gateway/build.rs`, `.github/workflows/release.yaml:1-8` — `HS_VERSION` still falls back to `git describe` / `CARGO_PKG_VERSION` and the scripts only rerun on `GITHUB_REF_NAME`, so local builds bake a stale tag (the rc.245 incident from CLAUDE.md, unmitigated); hs-mcp bakes no version; a tag push builds and publishes with no fmt/clippy/test gate (`ci.yaml` is separate) and no step compares `hs --version` to the tag. Fix: emit `rerun-if-changed` for `.git/HEAD` + refs or fail outside CI/tag builds, dedupe the copies, add `needs: check` to release plus a smoke `--version` step that fails the build on mismatch. **[fixed — residual: first real tag run untested; server binaries have no --version; CLAUDE.md release bullet stale]**
- **RA-74** `.github/workflows/release.yaml:63-64,72-124,7-9`, `.github/workflows/ci.yaml:36-43` — no `--locked`; `pip3 install ziglang` / `cargo install cargo-zigbuild` are unpinned in a job holding `contents:write, packages:write`; actions on mutable tags. Fix: `--locked`, pin versions/SHAs, per-job permissions. **[fixed — residual: pins freeze until bumped; rust:latest, ollama:latest, floating rust toolchain, un-checksummed Dockerfile pdfium]**
- **RA-75** `.github/workflows/ci.yaml:36`, `.github/workflows/release.yaml:95` — `cargo fmt` omits hs-gateway / hs-mcp / openalex-ingest / personal; release builds `hs-distill --features cuda`, which CI never compiles. Fix: `cargo fmt --all --check`; add a cuda `cargo check`. **[fixed]**

### P2

- **RA-76** `hs-common/src/service/registry.rs:74-88` — `discover_instances` maps no-creds, expired refresh token and gateway-down all to an empty Vec (root cause of P0-16's fake-empty dashboard). Fix: return `Result`. **[moot: discover_instances deleted (it had no caller)]**
- **RA-77** `hs-common/src/storage/config.rs:57-75` — `expand()` turns an unset `${VAR}` into "" (empty S3 creds, no error) and pushes bytes as `char` (non-ASCII mangled); `build()` never validates non-empty endpoint/bucket/keys. Fix: error on an unset var, iterate `chars()`, validate the S3 config. **[fixed]**
- **RA-78** `hs-common/src/auth/client.rs:50-55` — credentials are written with the default umask then chmod 0600 (world-readable window); `CloudCredentials` (`:13`) and `S3ConfigYaml` (`storage/config.rs:30`) derive `Debug` (+ `Serialize`) with secrets in them. Fix: `OpenOptions::mode(0o600)` at create; redacting `Debug`. **[fixed]**
- **RA-79** `hs-common/src/event_bus/nats.rs:67,201` — `async_nats::connect(url)` has no auth/TLS options, yet payload `key`s drive storage ops (RA-27); delivery errors are dropped to `warn` and the stream just ends (RA-47). Fix: `ConnectOptions` creds/TLS in `NatsYaml`. **[fixed]**
- **RA-80** `hs-common/src/logging/shipper.rs:18,66`, `logging/mod.rs:233-241`, scribe/distill `server_main.rs` `install_logging` (`if let Ok(storage)` ~`:89-90`) — `ship_interval_secs: 0` panics `interval(ZERO)`; failed ships stay in the spool forever (unbounded) and are re-dated to the ship day; config/storage failures silently disable shipping. Fix: validate ≥ 1, cap spool bytes/age, log once when disabled. **[fixed]**
- **RA-81** `hs-common/src/secrets.rs:42` (callers `crates/hs-scribe/src/server_main.rs:32`, `crates/hs-distill/src/server_main.rs:41`, `crates/hs-gateway/src/main.rs:28`, `crates/hs-mcp/src/main.rs:3528`) — `unsafe set_var` is justified as "before any tokio runtime starts" (`secrets.rs:39-41`), but all four servers call `load_default_secrets()` as the first line of an async main, i.e. after the multi-thread runtime is built, so the SAFETY comment is false; the `Result` is discarded with `let _ =`. Fix: call it in `main()` before the runtime builds and propagate the error. **[fixed]**
- **RA-82** `hs-common/src/service/lib_bootstrap.rs:96-103` — every pyke `dfbin/<platform>/<hash>` dir containing `libonnxruntime_providers_cuda.so` is prepended in unsorted `read_dir` order. With more than one hash (the documented CUDA12/13 bundle trap) which one loads is arbitrary → ABI segfault. Fix: pick exactly one deterministically or fail if > 1. **[fixed — residual: stale pyke CUDA bundles on hosts: pruning is an operator step]**
- **RA-83** `hs-common/src/markdown.rs:69,117` — `read_markdown_via` `.ok()?` collapses storage errors and invalid UTF-8 into "missing"; `resolve_markdown_key_verified` uses `unwrap_or(false)`. Same class as the fixed catalog P0-10. Fix: `Result<Option<_>>`. **[fixed]**
- **RA-84** `hs-common/src/html.rs:65` — `has_login && len < 100_000` rejects any short real HTML article carrying a "Sign in" nav link (scribe `event_watch.rs:359` makes it a permanent failure). Fix: gate on visible-text length or use only `is_known_interstitial`. **[fixed]**
- **RA-85** `hs-common/src/service/protocol.rs:51-60` + `crates/hs-scribe/src/client.rs:~340-365` — duplicate NDJSON readers; both rescan the whole buffer for `\n` per chunk (O(n²) on a multi-MB Result line) and only `warn` on unparseable lines. Fix: one shared incremental reader that errors on malformed lines. **[fixed]**
- **RA-86** `hs-common/src/auth/client.rs:15` — privacy per CLAUDE.md: a real gateway domain here (→ `<gateway-domain>`); a real S3 host:port and admin user name at `hs-common/src/storage/config.rs:140-143` (→ `<host>:<port>` / `<s3-user>`); a real home path in `crates/hs-scribe/src/backend_probe.rs:170` (→ `/home/<user>/…`); real usernames/paths in `crates/hs/src/restart_cmd.rs:819-926`, `crates/openalex-ingest/tests/snapshot_live.rs:11` and `crates/openalex-ingest/examples/*` (→ `/home/<user>`, `/Users/<user>`). Same sweep covers two non-atomic writes: `crates/hs/src/mcp_cmd.rs:141-150` overwrites the user's MCP client config in place, and `crates/hs/src/distill_cmd.rs:1008-1010` writes a multi-writer status file non-atomically. Fix: placeholders; temp file + rename for both writes. **[fixed — residual: README/docs still carry a private /24 subnet and NFS mount example]**
- **RA-87** `crates/hs-distill/src/event_watch.rs:140-168` — a failed embedding stamp (after retries) and a failed `distill.completed` publish are only logged and the handler ACKs; `serde_json::to_vec(..).unwrap_or_default()` would publish an empty payload. Recovery depends on a manual `hs distill reconcile`. Fix: return Transient (indexing is idempotent) and propagate serde errors. **[fixed]**
- **RA-88** `crates/hs-distill/src/chunker.rs:151-171,73-75` — after the final chunk reaches `text.len()` the loop re-enters at `len - overlap` and emits a last chunk entirely contained in the previous one (a duplicate vector per long segment); `chunk_max_tokens: 0` gives `max_chars = 0` and an infinite loop on a tokio worker; `find()` rescans from the segment start for every chunk (quadratic on large unsegmented markdown). Only single-chunk tests exist. Fix: break when `actual_end == len`, validate config (> 0, overlap < max), carry offsets. **[fixed]**
- **RA-89** `crates/hs-distill/src/qdrant.rs:197-201` — `build_filter` silently drops an unparseable `year` filter and returns unfiltered results; search `limit` (`server.rs:425`) and `/docs` `limit` (`:164`, default 100 000) are unbounded. Fix: 400 on a bad filter, clamp limits. **[fixed]**
- **RA-90** `crates/hs-distill/src/qdrant.rs:477-481,300` — `distinct_doc_count` silently saturates at 100 000 with an `exact` facet per `/status`; `collection_info().unwrap_or(0)` hides failures. Fix: use count/scroll or report truncation. **[fixed]**
- **RA-91** ONE-PATH leftovers in hs-distill — `crates/hs-distill/src/client.rs:273-275` 404 → non-streaming fallback; `embed/mod.rs:22-30` `FallbackEmbedder` with an unused `_fallback` arg; `server_main.rs:50` stale "GPU→CPU fallback" comment; `pipeline.rs:128-137` filesystem catalog walk (`unwrap_or_default()` → relative `catalog/`); `abstracts.rs:19-21` `TitleOnly` "last resort" degraded substitute (same chain at CLI level: RA-110). Fix: delete. **[fixed]**
- **RA-92** `crates/hs-distill/src/config.rs:46-56,108-125` — dead/ignored config: `host` / `port` (the server uses CLI defaults), `embedding.model`, `sparse_enabled` (embeddings are always `sparse: None`), and `dimension` (drives `ensure_collection` but the model is fixed at 1024 → silent mismatch). Fix: delete or validate against the probe output length. **[fixed]**
- **RA-93** `crates/hs-distill/src/server.rs:108-111` — `/readiness` is hard-coded `ready: true`, one slot; every handler error is HTTP 500, which `event_watch` treats as Transient. Fix: report real capacity; 4xx for bad input. **[fixed]**
- **RA-94** `crates/hs-distill/src/` (tests) — none for `index_document` (gates, stale chunks, skip stamping), server handlers, `ensure_collection`, `OnnxEmbedder`, or chunker multi-chunk / overlap / non-ASCII. Fix: add them alongside RA-40 / RA-41 / RA-42 / RA-44 / RA-88. **[fixed]**
- **RA-95** `crates/hs-scribe/src/event_watch.rs:233,347` — `(*source.bytes).clone()` deep-copies the whole PDF/HTML per backend attempt despite the `Arc` (overlaps CR-9's second bullet); QC (`:270-272`; `longest_repeated_run_bytes` builds a 16 B/char Vec) and HTML parse run synchronously on a tokio worker. Fix: share the `Arc` bytes into the request body, move QC/parse to `spawn_blocking`. (The EPUB decompression cap was split out as RA-15.) **[fixed — residual: longest_repeated_run_bytes still allocates 16 B/char]**
- **RA-96** `crates/hs-scribe/src/pipeline/processor.rs:487,614,745,826` — `0..total as u16` truncates page counts > 65 535 (silently converts `total mod 65536` pages); `pipeline/pdf_parser.rs:10` `Pdfium::default()` panics if libpdfium is absent (lib_bootstrap docs: "scribe panics on the first PDF"). Fix: reject with an error; `bind_to_library` + `?`. **[fixed]**
- **RA-97** `crates/hs-scribe/src/config.rs:178-185` — `AppConfig::load` reads `dirs::config_dir()/home-still/config.yaml` (not `~/.home-still/config.yaml`) with `.nested()` and no `.select()`, so the real config file cannot apply (only `HS_SCRIBE_*` env or defaults do) and load errors are swallowed (`server_main.rs:44`); at `:446-467` `Env::prefixed("HS_SCRIBE_")` is merged at root then `.focus("scribe")`, so documented overrides like `HS_SCRIBE_CONVERT_TIMEOUT_SECS` likely never apply to `ScribeConfig` (unverified: figment semantics inferred; verify with a one-off load). Fix: load from the real path, apply env at the right nesting, propagate errors (RA-6). **[fixed]**
- **RA-98** `crates/hs-scribe/src/classify.rs:45` — `msg.contains("paywall")` / `"FormatError"` etc. match text that can include event keys and stems (`scribe convert failed for {key}`), so a stem containing "paywall" is permanently failed; tests still cite a 200-char floor while `MIN_INDEXABLE_NON_WS = 50`. Fix: typed error codes over HTTP instead of substrings. **[fixed]**
- **RA-99** `crates/hs-scribe/src/ocr/repetition_detector.rs:83,184-191` — `check()` runs per SSE delta: 5 passes × ~256-word windows with a heap `Vec<&str>` HashMap key per n-gram (~10⁷ allocs per 8 k-token region) on the async runtime. Fix: slice/hash keys; check every N words. **[fixed]**
- **RA-100** `crates/hs-scribe/src/models/layout.rs:102`, `models/table_structure.rs:103` — `CUDAExecutionProvider::default().build()` without `.error_on_failure()`, so `use_cuda: true` silently runs on CPU on registration failure (distill already fixed this at `crates/hs-distill/src/embed/onnx.rs:104-108`); `table_structure.rs:171-186` trusts model vocab == `CHAR_DICT.len()` (index/slice panic with another model file) and `resize_w` can be 0 (`:120`). Fix: `error_on_failure`; validate output shapes at load. **[fixed]**
- **RA-101** `crates/hs-scribe/src/client.rs:259-267` — `/readiness` 404 becomes a synthetic "always ready" (legacy-server shim; masks a mis-routed gateway path); `server.rs:26-35` accepts any `X-Convert-Deadline-Secs` u64 with no server ceiling. Fix: delete the shim; clamp the deadline. **[fixed]**
- **RA-102** `crates/hs-scribe/tests/` — nothing covers `prepare_source` / `convert_and_upload` stamping, olmocr failed-page accounting, readiness semantics, or config validation; `repro_2col_test` is `#[ignore]`. Fix: add tests alongside RA-48 / RA-51 / RA-54. **[fixed — residual: libpdfium-dependent tests skip silently; fault-exit/preflight wiring untested]**
- **RA-103** `crates/hs-scribe/Cargo.lock` — stray lockfile in a workspace member; `crates/hs-scribe/src/config.rs:13-35` is a third hand-rolled YAML `project_dir` scanner (also `hs-common/src/lib.rs:20-48,91-116`, P2-13). Fix: delete the lockfile; one serde-based loader. **[fixed]**
- **RA-104** `crates/hs-gateway/src/enrollment.rs:45,80`, `oauth.rs:169,281,451` — no rate limiting on `/cloud/enroll`, `/authorize`, `/token`, `/register` (codes ≈ 2^30, `token.rs:157-166`); `/register` and `auth_codes` stores are unbounded; std `Mutex::lock().unwrap()` poison panics. Fix: tower-governor/limits, TTL reaping, bounded maps. **[fixed]**
- **RA-105** `crates/hs-gateway/src/main.rs:1,57-58,119-123` — `#![allow(dead_code)] // WIP`, TODO `cf_access` creds, shutdown only on Ctrl-C (SIGTERM from systemd skips flush); tests cover only resolve_service/config/PKCE (nothing for admin invite, scope checks, registry ownership). Fix: remove the allow, handle SIGTERM, add auth tests. **[fixed]**
- **RA-106** `crates/hs-gateway/src/proxy.rs:~72` — `backend_url` is built from `uri().path()` only: query strings are dropped for any proxied GET. Fix: use `path_and_query()`. **[fixed]**
- **RA-107** `crates/hs-mcp/src/main.rs:2747-2749,2917` — `publication_year >= COALESCE(?,0)` drops works with a NULL year even with no filter. Fix: `(? IS NULL OR …)`. **[fixed]**
- **RA-108** `crates/hs-mcp/src/main.rs:572,645` — a single `std::sync::Mutex<Connection>` serializes all OpenAlex queries (one slow BM25 blocks all), `lock().unwrap()`; ~30 `to_string_pretty(..).unwrap_or_default()` return an empty "success" (e.g. `:478,759,781`); `.ok()` on health/status hides causes (`:1758,2073,2993`); the description at `:2259` references the removed `distill_purge`. Fix: `try_clone()` per request, `map_err`. **[fixed — residual: hs-mcp health/status .ok() still hides causes in the snapshot; read-only try_clone and fts autoload unconfirmed]**
- **RA-109** `crates/hs/src/openalex_cmd.rs:48-49,95-120,185-187` — every subcommand (incl. `query` / `status`) opens the DB read-write (creates the file, applies DDL, 24 GB pragma): "read-only SELECT" accepts DDL/DML and blocks against hs-mcp's read-only handle; `seen_set` falls back to `/tmp`. Fix: `AccessMode::ReadOnly` for query/status; no `/tmp` fallback. **[fixed]**
- **RA-110** `crates/hs/src/distill_cmd.rs:~275-330` — abstracts are built from an OpenAlex → catalog → markdown → title-only chain (degraded `title_only` embeddings stored), `.ok().flatten()` hides DB errors, blocking DuckDB runs in async, and it returns `Ok(())` despite errors. Fix: one source, fail rows loudly. **[fixed]**
- **RA-111** `crates/hs/src/status_cmd.rs:921-922`, `crates/hs/src/main.rs:190-196`, `crates/hs/src/scribe_inbox.rs:290,181,499` — non-async-signal-safe SIGINT handler (self-admitted); `select!{ctrl_c}` cancels destructive commands mid-loop with no summary and makes the inbox's flag handler dead; unknown mtime ⇒ `UNIX_EPOCH` (files still uploading are processed); the test at `scribe_inbox.rs:499` has `matches!(..)` with no assert. Fix: fix the signal path, assert in the test. **[fixed]**
- **RA-112** `crates/openalex-ingest/src/reader.rs:81-86`, `seen_set.rs:61,111-125` — a persistent line IO error just `continue`s (possible tight loop on a corrupt gz; unverified), checkpoint `flush()` without `sync_all`, partial trailing bytes silently accepted, `create_dir_all().ok()`. Fix: return the error; fsync before rename. **[fixed]**
- **RA-113** `crates/openalex-ingest/src/duckdb_loader.rs:77-82,133,803+,623-624` — `PRAGMA temp_directory='{}'` and `read_json('{glob}')` are built with `format!` (a quote in the path breaks/injects); dimension loaders use `INSERT OR IGNORE`; `INSTALL fts` hits the network; comments say 14 GB vs the 24 GB pragma; `serde_json … unwrap_or_else("[]")`. Fix: parameterize/escape, bundle FTS. **[needs-decision: INSERT OR IGNORE dimension loaders and runtime INSTALL fts remain (owner decisions)]**
- **RA-114** `crates/openalex-ingest/src/lookup.rs:~38` — `openalex_id = ? OR doi = ?` likely scans 56M rows per call (called per catalog entry from `crates/hs/src/distill_cmd.rs:~275`); DOIs are stored case-sensitive while paper lowercases. Fix: separate indexed lookups; lowercase at ingest. **[fixed — residual: operator must confirm no mixed-case DOIs in works (else expression index or re-ingest)]**
- **RA-115** `crates/openalex-ingest/tests/` — no tests for `load_works*`, SeenSet/failed-partition interplay (RA-8) or exit codes; live tests/examples hard-code home paths (RA-86). Fix: add a failure-injection test. **[fixed]**
- **RA-116** `paper/src/config.rs:79,127,146`, `paper/src/providers/downloader.rs:136,199,201`, `paper/src/providers/semantic_scholar.rs:~275-400` — `Env::split("_")` breaks every multi-word key; zero `rate_limit_interval_ms` panics at `paper/src/resilience/rate_limiter.rs:12`, `max_concurrent=0` hangs `buffer_unordered(0)`; default `http://` arXiv/OpenAlex; fake NCBI email; unencoded DOI/email in URLs; the citations loop follows `next` without a progress guard. Fix: validate config, https, `Url` builders. **[fixed]**
- **RA-117** `.github/workflows/release.yaml:249-258,322,275-306`, `.github/workflows/e2e.yaml` — releases are not marked `prerelease` (so `/releases/latest` returns an rc and `--pre` is meaningless), the `:latest` image moves on rc tags, no `concurrency` group, two docker build paths (amd64 prebuilt vs arm64 from source), x86_64-linux on floating `ubuntu-latest` glibc, Windows builds of gateway/mcp that are never packaged; e2e interpolates `inputs.image_tag` into shell, downloads ONNX models without a checksum, and the Ollama wait loop never fails. Fix: set prerelease for `-rc`, quote inputs, verify hashes. **[needs-decision: rc releases not marked prerelease and :latest moves on rc tags (owner decisions); rest fixed]**

### Operational follow-ups (not code)

Operator steps the workstreams surfaced. None was performed by the audit (no live host, config, service, GPU or database was touched). Each is a story with a one-line acceptance. Order for a rollout: binaries first, then config (older binaries reject new strict keys such as `events.nats.drain_timeout_secs`).

- **RA-OPS-1** Provision `HS_BACKEND_TOKEN` (≥ 32 visible ASCII bytes, e.g. `openssl rand -hex 32`) in `~/.home-still/secrets.env` (0600) on the gateway host, every scribe/distill/hs-mcp host and every host that runs `hs` / watchers / `personal` **before** upgrading; scribe, distill, hs-mcp `--serve` and the gateway refuse to start without it. Acceptance: all five services start; an unauthenticated request to a non-probe route returns 401 and `hs status` is green via the gateway.
- **RA-OPS-2** Gateway rollout: set `--gateway-url https://<public-host>` on the gateway unit and `cloud.gateway.routes` (keys `scribe|distill|mcp`, bare `http(s)://host[:port]` URLs or lists) BEFORE upgrading, because the registry is gone and startup fails without them; upgrade `hs-gateway` and `hs` on the gateway host together, confirm the unit actually restarted onto the new binary, then re-enroll every device (`hs cloud invite --name <device>` on the gateway host, `hs cloud enroll --gateway https://<public-host>` on the device) and every OAuth client (old tokens lack `typ`). Remove stale `cloud.role` / `cloud.gateway_url` keys and fix the README client example. Acceptance: `curl http://127.0.0.1:<port>/health` on the gateway host and `hs cloud status` on each client succeed; old tokens are refused.
- **RA-OPS-3** Enable HNSW on the three existing Qdrant collections in a maintenance window: `hs distill hnsw enable --collection <name> --yes` for `academic_papers`, `paper_abstracts`, `personal_docs` (they were created with `m=0`; the server logs `HNSW is DISABLED` at ERROR on every start until done; Qdrant rebuilds segment by segment, CPU- and ~1× disk-heavy; `hnsw.max_indexing_threads` caps it). Acceptance: no `HNSW is DISABLED` ERROR on distill restart and search latency drops.
- **RA-OPS-4** Re-embed the corpus after the embedding window change (RA-40): every vector written before holds only the first 512 tokens of its chunk. Re-index incrementally (idempotent since RA-42; stale tails disappear per document). Acceptance: a sampled long document returns a hit for text from the back half of a chunk.
- **RA-OPS-5** Check whether the production `seen_set.bin` was poisoned by the old loader (IDs with no row in `works`): move it aside (do not delete), stop everything holding the DuckDB file, run `hs openalex load-works` (the new `SeenSet::open` refuses a checkpoint ahead of `works` and rebuilds from `works`), review partitions without an `ok` row in `_ingest_log`, and check orphan edges (`work_authorships` without a `works` row). Acceptance: load-works completes with exit 0 and no partition lacks an `ok` row.
- **RA-OPS-6** On the live OpenAlex DB compare each dimension partition's JSONL line count with `_ingest_log.rows` (`INSERT OR IGNORE` may have dropped duplicate IDs; oldest partition wins). Acceptance: no shortfall, or a recorded decision (RA-OPS-D8).
- **RA-OPS-7** Run `SELECT count(*) FROM works WHERE doi <> lower(doi)`. Acceptance: 0; otherwise add a `lower(doi)` expression index or re-ingest (query-side lowercase in `lookup.rs` assumes lowercase storage).
- **RA-OPS-8** NATS durable consumers: (a) a watcher no longer recreates its consumer, it updates drift in place with a WARN; `ack_policy`/`deliver_policy` are immutable, so a stale consumer with a different policy needs a manual `nats consumer rm` (watcher fails every start until then); (b) after a crash or exit-70 up to `max_ack_pending` pulled-but-unstarted events wait `ack_wait` (2 h) before redelivery (orderly stops NAK them back); (c) roll out binaries before config (`events.nats.drain_timeout_secs` is rejected by older binaries); (d) never exercised against a real JetStream: watch the first restart. Acceptance: after restarting both watchers there is one consumer per durable and its settings match config.
- **RA-OPS-9** Size `MemoryMax`/`MemoryHigh` on scribe units (docs/deployment.md has the recommendation; nothing is generated): a referenced compressed object-stream PDF costs ~2.5 GB for a 1.5 MB file in both pdfium and lopdf (RA-14 residual); the wedge watchdog exits 70, which restarts every conversation in flight. Acceptance: `systemctl show hs-serve-scribe*.service -p MemoryMax` is set on every scribe unit and the unit restarts on exit 70.
- **RA-OPS-10** Install libpdfium on every host that runs `hs-scribe-server` or `hs scribe watch-events`, olmocr hosts and macOS (drop dir or system path) included; both refuse to start without it. CI should also fail instead of skipping pdfium tests (`HS_REQUIRE_PDFIUM=1`, RA-133). Acceptance: `ldconfig -p | grep pdfium` (Linux) / `libpdfium.dylib` in a drop dir (macOS) on each host and both processes start.
- **RA-OPS-11** On hosts with a custom `home.project_dir` and no `storage:` section, set `storage.backend: local` + `storage.local.root` explicitly: the local root defaults to `~/home-still`, not `project_dir` (one WARN names both paths). Acceptance: the WARN is gone and the root matches the intended data directory.
- **RA-OPS-12** Legacy per-region scribe hosts: verify `/health` shows both ONNX models loaded BEFORE upgrading; a missing layout/table model is now fatal at startup. Acceptance: `/health` shows both models on each Legacy host.
- **RA-OPS-13** Host config hygiene before upgrading (the binaries now refuse or warn): `events:` with an explicit `backend` (`nats` for watchers; `hs-mcp` refuses without it); `scribe.servers` / `distill.servers` on every host that runs those clients; no unknown keys under `storage:` (only `backend`, `local.root`, `s3.{endpoint,bucket,region,access_key,secret_key,allow_http}`), `home:` (only `project_dir`, `log_dir`) or `events.nats`; `secrets.env` readable; `logs.*` knobs ≥ 1; distill limits (`embedding.batch_size` ≤ 20, `chunk_max_tokens` ≤ 1232, `dimension` 1024, `index_timeout_secs + 120 ≤ events.nats.ack_wait_secs`, `personal.collection_name` listed in `distill_server.collections`); an `openalex.api_key` must not be combined with an `http://` base URL; scribe server settings live under `scribe_server:` or `HS_SCRIBE_*` env, not `~/.config/home-still/config.yaml`. Acceptance: `hs config path` + a start of each binary on the host succeeds with no config error.
- **RA-OPS-14** Remove stale `HS_SCRIBE_TIMEOUT_SECS` from unit files (never did anything; now WARNs; real knobs `HS_SCRIBE_VLM_IDLE_TIMEOUT_SECS`, `HS_SCRIBE_OLLAMA_REQUEST_TIMEOUT_SECS`), and optionally set `HS_SCRIBE_VLM_CONCURRENCY=2` on the olmocr unit; leftover `Environment=HS_ADVERTISE_IP=` is harmless. Acceptance: no "environment variable sets nothing" WARN in the journal.
- **RA-OPS-15** Purge legacy title-only abstracts (catalog rows with `abstract_embed.source == "title_only"` and their `paper_abstracts` points). Acceptance: `hs distill abstracts status` reports `title_only=0`.
- **RA-OPS-16** Prune stale pyke CUDA bundles on hosts with more than one `dfbin/<platform>/<hash>` (the loader now picks the oldest deterministically and prints one line when > 1 exist). Acceptance: exactly one CUDA bundle per platform.
- **RA-OPS-17** `hs upgrade` over ssh needs `XDG_RUNTIME_DIR=/run/user/$(id -u)` (unreachable `systemctl --user` now fails the restart phase after the binaries were replaced); the first upgrade FROM a pre-RA `hs` still runs the old installer (no checksum), so verify that one by hand. `hs upgrade` now refuses releases without `<asset>.sha256`. Acceptance: upgrade exits 0 on a multi-unit host with all units restarted.
- **RA-OPS-18** Watch the first tag run on GitHub (tags are immutable, no dry run): it is the first execution of the reusable-workflow gate, pinned zig/zigbuild, the from-source amd64 docker build and every verify step. Adopt a bump schedule for the frozen action/zig pins (Dependabot or manual). Local `--release` builds now need `HS_RELEASE_TAG=vX.Y.Z[-pre]`. Acceptance: the tag's release job is green and `hs --version` equals the tag on every published asset.
- **RA-OPS-19** Consumers of changed contracts: `paper_search` now always returns `{papers, provider_failures}`; `hs pipeline *`, `hs migrate *`, `hs distill abstracts build|reconcile` exit non-zero on any item error (cron/scripts that ignored the code will start seeing failures); `scribe_convert` re-announces existing markdown instead of re-converting. Acceptance: callers updated, cron jobs checked.
- **RA-OPS-D1** DECISION RA-61: keep checksum-only `hs upgrade` (checksum comes from the same release as the archive) or add signed releases (minisign/cosign, pinned public key; no signing infra exists).
- **RA-OPS-D2** DECISION RA-117: release channel policy: mark `-rc` releases `prerelease` and move plain `hs upgrade`/`install.sh` off `/releases/latest` (or introduce a stable `v0.1.0` line), and decide whether `:latest` moves on rc tags (add an `:rc`/`:edge` tag). Order: client change must ship fleet-wide before flipping `prerelease`.
- **RA-OPS-D3** DECISION RA-24/F10: one static shared `HS_BACKEND_TOKEN` over plain HTTP between gateway and every backend (blast radius = all services; `AuthedHttp::plain` sends it to any configured URL) versus per-service tokens and/or TLS/mTLS.
- **RA-OPS-D4** DECISION RA-36: add `put_if_absent` to the `Storage` trait (S3 `If-None-Match: *`, LocalFs `create_new`) for create-only moves, or keep the accepted head→put race (conditional-write support on Garage unverified).
- **RA-OPS-D5** DECISION RA-74: pin the Dockerfile pdfium download by checksum (digests exist: linux-arm64, mac-arm64, win; the amd64 build verifies only in CI) and pin `rust:latest`, `ollama/ollama:latest`, the rust toolchain.
- **RA-OPS-D6** DECISION RA-2/RA-18: redirect-host allow-list for open dynamic client registration; refresh-token rotation and reuse detection.
- **RA-OPS-D7** DECISION RA-26: atomic collection reset via alias-versioned collections (needs migrating the existing collection names) or leave reset drop+recreate behind the token.
- **RA-OPS-D8** DECISION RA-113: `INSERT OR IGNORE` on dimension loaders (silently drops duplicates, oldest wins) and `INSTALL fts` network access: keep, make an explicit operator step, or vendor the extension.
- **RA-OPS-D9** DECISION RA-35/RA-33/RA-56: the per-partition parse-error threshold (100), whether rows the appender refuses (out-of-range year, bad date) should abort the load or be skipped, and whether a lost catalog stamp after a committed conversion should be repaired in the `AlreadyConverted` path.

### Observed, not fixed (new findings)

Found by the workstreams and reviewers while fixing the above; each re-verified by reading the code at tip `ef207f88af84`. Same priority scale as the section above.

- **RA-118** `hs-common/src/catalog.rs:~494` — `list_catalog_entries_parallel` fails on storage errors now, but still skips rows that do not parse as `CatalogEntry` (`if let Ok(entry) = serde_yaml_ng::from_slice(..)`), so a corrupt row vanishes from every listing, status and drift count. Fix: return an error (or a per-row error list) for unparsable rows.
- **RA-119** `hs-common/src/status.rs:1093` — `read_inbox_heartbeat` collapses storage errors, parse errors and a missing object into `None` (`.ok()?`, `.ok().flatten()?`), so "inbox watcher dead" and "storage down" look identical. Fix: `Result<Option<_>>`.
- **RA-120** `hs-common/src/lib.rs:127` — `collect_files_recursive` ignores `read_dir` errors and unreadable entries and returns a partial list. Fix: return `Result`.
- **RA-121** `crates/hs-mcp/src/main.rs:417`, `crates/hs/src/{distill_cmd,openalex_cmd,cloud_cmd}.rs`, `GatewayConfig::load` — read `~/.home-still/config.yaml` with their own serde code instead of `ConfigFile` (they fail loudly, but hs-mcp's caller degrades a malformed `openalex:` section to "not configured"). Fix: one loader; fail startup on a malformed `openalex:` section when OpenAlex tools are enabled.
- **RA-122** `crates/hs-scribe/src/config.rs`, `crates/hs-distill/src/config.rs` — unknown keys inside `scribe:`, `scribe_server:`, `distill:`, `distill_server:` are silently ignored (only `events`, `storage`, `home` are strict), so `vlm_concurency` has no effect; nested env overrides (`HS_SCRIBE_TIMEOUT_POLICY_FLOOR_SECS`) only warn. Fix: deny unknown keys per section.
- **RA-123** `crates/hs-gateway/src/config.rs` — `GatewayConfig` has no `deny_unknown_fields`, so a typo in `cloud.gateway.*` (e.g. `max_concurrent_proxy_request`) is ignored. Fix: derive the key set from the struct (single source) and reject unknown keys.
- **RA-124** `personal/src/converters/docx.rs:10` — `docx_rs::read_docx(&bytes)` has no expanded-size, entry-count or nesting bound (same family as RA-15; operator-supplied files). Fix: bound before parsing, as `EpubLimits` does.
- **RA-125** `paper/src/providers/downloader.rs:347-365` — a failed `papers.ingested` publish after a stored download is only `warn!`ed, and `serde_json::to_vec(..).unwrap_or_default()` would publish an empty payload (also existing P0-17). Fix: propagate (fixing it changes retry semantics: a retry returns `skipped` without publishing, so republish on the skipped path).
- **RA-126** `crates/openalex-ingest/src/duckdb_loader.rs:1018-1066` — dimension loaders use `INSERT OR IGNORE` (the same ON-CONFLICT idiom the project removed for works): duplicates inside/across partitions are dropped silently and, with the oldest-first walk, the OLDEST version wins. Fix: plain INSERT inside the partition transaction plus an explicit duplicate check, or document and count the drops (decision RA-OPS-D8).
- **RA-127** `crates/openalex-ingest/src/duckdb_loader.rs:203` — `INSTALL fts` runs at runtime and needs network on a fresh host (libduckdb-sys has no bundled fts). Fix: explicit operator step or vendored per-arch extension (decision RA-OPS-D8).
- **RA-128** `crates/openalex-ingest/src/duckdb_loader.rs:676-722` — authors partitions are appended straight into the live table (no stage → merge-transaction path like works), so a failed partition leaves flushed rows and the re-run fails on the first duplicate key. Fix: stage and merge in one transaction.
- **RA-129** `hs-common/src/event_bus/config.rs:115` — `events.nats.max_deliver`, `ack_wait_secs`, `max_ack_pending` and watcher concurrency are not range-validated (only `url`, `drain_timeout_secs` and the auth/TLS keys are). Fix: reject zero/negative and `ack_wait` below the distill index timeout at load.
- **RA-130** `hs-common/src/event_bus/nats.rs:219` — `events.nats.url` may embed `user:pass@`, and `connecting to NATS at {url}` logs it; `validate` does not reject userinfo. Fix: reject userinfo and redact the URL in logs.
- **RA-131** `crates/hs/config/default.yaml:89` — the `hs config init` template writes `events.backend: noop`, so a freshly initialised host runs `hs-mcp` / `hs paper download` with publishes silently dropped until the operator edits it. Fix: leave the key commented so the first run forces a choice.
- **RA-132** `crates/hs-scribe/src/pdfium.rs` (`with_parser`, process-wide pdfium lock) — Legacy conversions are effectively serialised at the render stage (the lock is held for a whole conversion), and a pdfium call that never returns cannot be killed in-process (the watchdog exits the process, restarting every conversation in flight). Fix: child-process isolation, or an explicit decision that Legacy concurrency is 1.
- **RA-133** `crates/hs-scribe/src/server_main.rs:62`, `crates/hs/src/scribe_cmd.rs:372`, `hs-common/src/event_bus/nats.rs` (`ensure_consumer`), `crates/hs-mcp` LazyBus, `build-support/version_marker.rs` — the exit-70 fault hook, the drain-timeout pass-through, `ensure_consumer` (get-or-create/update/immutable-field failure), LazyBus concurrent first use and `keep_version_marker()` in each `main` have no test that fails when the wiring is removed; `skip_without_pdfium!` tests pass vacuously without libpdfium (no `HS_REQUIRE_PDFIUM=1` in CI); fault-exit tests use a 700 ms grace, not the production 10 s / 60 s budgets. Fix: wiring tests (child-process), CI `HS_REQUIRE_PDFIUM=1`.
- **RA-134** `crates/hs/src/distill_cmd.rs:537,621,842,922,1000` — `reqwest::get(.../healthz)` has no timeout; a hung Qdrant blocks `hs distill status/start`. Fix: use `hs_common::http::client_builder()` with a timeout.
- **RA-135** `crates/hs-mcp/src/main.rs:2121` — only `scribe_convert` sends a progress heartbeat; `distill_backfill`, `catalog_backfill_title`, `personal_add`, `distill_scan_repetitions` run longer than `--session-idle-timeout-secs` (3600) without progress and are cancelled with their session. Fix: heartbeat in every long tool.
- **RA-136** `crates/hs-scribe/src/ocr/cloud.rs:44`, `crates/hs-scribe/src/client.rs:449` — unbounded `resp.json()` / `resp.text()` of a third-party / peer reply. Fix: size-capped body reads.
- **RA-137** `crates/hs-scribe/src/pdfium.rs:268` — `./libpdfium.so` is tried before the drop dirs and system path, so `hs scribe convert` run from an untrusted directory loads whatever library is there. Fix: drop the cwd candidate or put it last.
- **RA-138** `README.md:432`, `docs/deployment.md:183,337` — committed docs carry a private RFC 1918 /24 subnet example in NFS-export and firewall snippets and a real-looking NFS mount path (CLAUDE.md privacy rule; the RA-86 sweep missed them). Fix: `<lan-subnet>` / `<nfs-mount>` placeholders.
- **RA-139** `crates/hs/src/distill_cmd.rs:1872` — `distill reconcile --reembed` does `let _ = update_embedding_skip_via(..)` on the failure path (the failure itself is counted and fails the command, but a lost skip stamp is invisible). Fix: count/log the stamp error.
- **RA-140** `crates/hs-scribe/docker/Dockerfile:1,14-21`, `crates/hs-scribe/docker/docker-compose.e2e.yml:3`, `.github/workflows/{ci,release}.yaml` — still floating: `rust:latest`, `ollama/ollama:latest`, `dtolnay/rust-toolchain@stable`, apt ghostscript; the pdfium tarball download in the Dockerfile has no checksum. Fix: pin by digest/version and verify (decision RA-OPS-D5).
- **RA-141** `crates/hs-gateway/Cargo.toml:22` (`figment`), `crates/hs/Cargo.toml:28` (`notify`) — unused dependencies (no source uses them). Fix: remove.
- **RA-142** `crates/hs-scribe/src/epub.rs:349-379` — EPUB spine items whose entry is missing or not UTF-8 are skipped with one warning, so a partial book can be stamped converted. Fix: fail the book (or record the skipped count in the QC verdict like skipped regions).
- **RA-143** `Cargo.toml:46` (`panic = "unwind"`), `crates/hs-mcp/src/main.rs` (rmcp session worker), background tasks (heartbeats, spool rotator) — under unwind a panic in an rmcp tool call ends that session task (the HTTP catch layer cannot see it) and a panic in a long-lived background task ends it silently while the process lives. Fix: supervise/log-and-restart or exit on background-task panic; catch inside tool handlers.
- **RA-144** `crates/hs-mcp/src/main.rs:3019-3021,3101` — `.ok()` on health/readiness/status calls discards the cause in the system snapshot (`unhealthy` with no reason beyond the activity line). Fix: carry the error text into the snapshot.
- **RA-145** `crates/hs-scribe/src/postprocess.rs:213` — `longest_repeated_run_bytes` builds a 16 B/char Vec for QC on a blocking thread (now in `spawn_blocking`, still memory-heavy on large markdown). Fix: suffix-array/rolling-hash scan without the per-char Vec.
- **RA-146** `README.md:120,202`, `crates/hs/README.md:12`, `crates/hs-scribe/README.md:25,37` — document a `hs scribe init` subcommand that does not exist (nothing generates a scribe compose file, so there is nothing to provision `HS_BACKEND_TOKEN` into). Fix: remove the mentions or implement the command (with token provisioning).
- **RA-147** `crates/hs-gateway/src/oauth.rs`, `crates/hs-gateway/src/auth.rs` — (security, P2) dynamic client registration is open (any https `redirect_uri` registers; the consent page is the only mitigation) and refresh tokens are not rotated or reuse-detected (decision RA-OPS-D6). Fix: redirect-host allow-list at `/register`; rotate and revoke the chain on reuse.
- **RA-148** `hs-common/src/storage/config.rs:20-24` — `LocalConfig::default()` roots local storage at `~/home-still` even when `home.project_dir` is set and `storage:` is absent (one startup WARN names both paths; deliberate, not moved). Fix: derive the default root from `home.project_dir`, or refuse to start when the two disagree.
- **RA-149** (P1) `crates/openalex-ingest` on Windows — after a DuckDB write fails (duplicate key in the authors appender, failed works merge) the next DuckDB call in the process never returns (windows-2022 CI, 2026-10-02; Linux/macOS fine). `hs openalex load|load-works|build-fts|build-indexes` now refuse on Windows before opening the database (`crates/hs/src/openalex_cmd.rs`), and `crates/openalex-ingest/tests/load_works.rs` is not built there. Fix: find the DuckDB-level cause (debug workflow on a `debug/*` branch running the failure-path tests one at a time) and lift the refusal.
