# home-still — Backlog

Prioritized stories from the 10-agent codebase audit. Ordered highest priority → lowest.
Security and documentation stories are intentionally excluded.

## Working rules for every story in this file

- **Greenfield — no backwards compatibility.** When a story says "delete", it means delete the code, not wrap it in a feature flag, not keep a legacy alias, not leave a deprecation stub. Callers get fixed in the same PR.
- **ONE PATH per feature.** No fallbacks, legacy shims, stub placeholders, "backup" modes, rollover behavior, compatibility branches. When the primary path can't produce a usable result, fail loudly. If a fix needs a "fallback" to work, the fix is wrong — redesign.
- **No CPU fallback.** Anywhere the audit says "tie to compute_device" or "fail if CUDA missing", the answer is fail — not a CPU path.
- **Definition of Done:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test` all pass before tagging any `rc.*`.

---

## P0 — Non-negotiable violations (must fix before next rc.*)

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

## P1 — Reliability (panics and silent failures in hot paths)

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
