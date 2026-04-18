# Claude Desktop Self-Test Prompt — home-still

Please stop and write your report at the first issue.  DO NOT CONTINUE TESTING.

Paste the prompt below into Claude Desktop with the `hs-mcp` server connected. It exercises the MCP tool surface and emits a debug-focused report covering the topology described in `docs/deployment.md`.

> **Companion check:** this prompt covers what's reachable through MCP. For LAN/host-side verification (NFS export, Garage admin port, Qdrant gRPC, NVIDIA driver, gateway systemd unit), run the shell commands in `docs/deployment.md` §9 in parallel. The two together are the full self-test.

> **CUDA is non-negotiable.** Per `CLAUDE.md`: "Distill MUST run on CUDA. Do not 'fall back to CPU' for distill / embedding — not as a quick fix, not temporarily, not as a workaround." This prompt fails the whole report if any distill instance reports a non-CUDA `compute_device`.

---

You have access to the `home-still` MCP server (tools prefixed with `paper_` / `catalog_` / `markdown_` / `scribe_` / `distill_` / `system_`). This is a vector-search academic research pipeline: OpenAlex / arXiv / PMC / Crossref / Semantic Scholar / CORE → PDF download → VLM scribe (markdown) → CUDA distill (Qdrant embeddings) → semantic search.

**Run an end-to-end self-test and produce a debug report.** Capture every error verbatim and continue to the next step. Only stop early if Phase 1 reveals the cluster is fundamentally unreachable (no MCP tool returns at all). Prefer small `limit` values; we want signal, not volume.

## Preflight — tool surface and schema sanity

Before Phase 0. These three checks confirm the `hs-mcp` binary Claude Desktop spawned is the rc this prompt was written against — not a stale local binary from a prior rc. Clients host `hs-mcp` via stdio and cache the child process at app launch, so a cluster-side upgrade is silently useless until the client session is bounced. If any preflight check fails, stop, restart Claude Desktop (`Cmd+Q`, reopen) or reconnect the `home-still` server in the MCP panel, and re-run preflight. Don't proceed to Phase 0 until all three pass.

a. **Tool surface.** Ask: *"What `home-still` MCP tools do you see? I'm specifically checking for `distill_scan_repetitions` and `catalog_repair`."* Both must be present. Absence means the spawned `hs-mcp` binary predates the current rc — the client-spawned binary isn't in sync with the cluster.

b. **Schema fields.** Ask: *"Call `system_status` and show me the `pipeline` object."* Both `pipeline_drift` and `pipeline_drift_threshold` must be present. Same diagnosis as (a) if missing: stale client binary.

c. **Arithmetic spot-check.** Compute `documents − markdown − conversion_failed − Σ(scribe.in_flight)` client-side and compare to the server's `pipeline_drift`. They must match. Divergence means the server-side formula has changed vs what this prompt documents — file an issue rather than debugging the cluster from a moving spec.

Only after all three pass: proceed to Phase 0.

## Phase 0 — Topology snapshot (read-only)

Before any pipeline work, establish what cluster you are looking at. None of these steps are pass/fail on their own — they make every later finding interpretable.

1. `system_status` (no args). From the snapshot, record:
   - **Storage backend** in use — infer from object key shapes returned by `markdown_list` / `catalog_list` later (S3 keys vs local POSIX paths).
   - **Service registry**: count and per-host URL of `scribe_instances` and `distill_instances`, with each one's `healthy`, `version`, `compute_device`, `embed_model`, `collection`, `activity`, `in_flight`, `slots_available`/`slots_total`.
   - **Qdrant rollup**: `qdrant.collection`, `qdrant.qdrant_url`, `qdrant.qdrant_version`, `qdrant.compute_device`.
   - **Pipeline counts**: `documents`, `pdfs`, `html_fallbacks`, `markdown`, `catalog_entries`, `conversion_failed`, `embedded_documents`, `embedded_chunks`.
2. Note whether you reached MCP **directly on-LAN** or **through the gateway** (token / origin clue from your client config). Record which path is exercising the rest of this test.

## Phase 1 — Health gates (read-only, hard pass/fail)

3. **CUDA gate.** For every entry in `distill_instances`, assert `compute_device == "Cuda"`. Also call `distill_status` and confirm the same. If any instance reports `Cpu` (or empty/unknown), mark **FAIL — CUDA NON-NEGOTIABLE VIOLATED** at the very top of the final report and name the offending instance URL. Do not skip the rest of the test — keep going so the report is complete — but the overall verdict is FAIL.
4. **Scribe gate.** `scribe_health` for each scribe instance. Server up? GPU name + utilization, queue depth, last conversion timestamp, slot availability. A scribe instance reporting CPU-only compute is also a FAIL per the same rule.
5. **Qdrant gate.** From `distill_status`: collection exists? Vector count plausible (non-zero on a populated cluster)? Indexed vs pending. URL matches what `system_status` reported.
6. **Pipeline math.** The snapshot carries two fields that make this a single-assertion gate:
   - `pipeline.pipeline_drift` — computed server-side as `documents − markdown − conversion_failed − (sum of scribe in_flight)`. Saturating subtraction, so never negative.
   - `pipeline.pipeline_drift_threshold` — the threshold above which the gate fails (currently 3).
   Acceptance: **`pipeline_drift <= pipeline_drift_threshold` is PASS.** Anything above is FAIL — report both values and which stage is likely over-counting. The absence of either field in the snapshot is itself a FAIL (the MCP server is older than this prompt expects).
   If there's an operator running `hs scribe inbox` on a client, note it: on a healthy cluster with an active inbox watcher, the drift balances within seconds of any drop into `papers/manually_downloaded/`.

## Phase 2 — Inventory & gap detection (read-only)

7. `catalog_recent` with a generous window — last 10 downloads, 10 conversions, 10 embeds. Report timestamps and call out gaps (downloaded but never converted, converted but never embedded, embedded with zero chunks).
8. `catalog_list` (page 1, small limit). Sample the first 5: confirm each has `markdown` and `embedded` flags set consistently with what `system_status` reported in aggregate.
9. `markdown_list` (small limit). Pick any 2 entries and `markdown_read` their first 500 chars — coherent text, not gibberish? Tables/equations preserved?
10. `distill_reconcile` with `{ "dry_run": true }`. Lists doc_ids that exist in Qdrant but have no markdown. The reconciler now consults `catalog_entry.markdown_path` before deciding something is orphaned, so pre-rc.241 unsharded rows no longer show up as ghosts. **Expected orphan count at steady state: ≤ 5.** Anything higher is a real anomaly — feed the offending doc_ids into `distill_scan_repetitions` / `distill_purge` for triage.
11. `distill_scan_repetitions` with `{ "dry_run": true, "limit": 1000 }`. Walks every markdown object and counts VLM repetition truncations; docs above the threshold (default 20 truncation sites) are flagged. **Expected `flagged_count == 0` at steady state.** Any hits mean the scribe-side QC gate let a loopy convert through — list the offending stems and their first offending snippet under a "Repetition poisoning" subsection of the report.
12. `catalog_repair` with `{ "dry_run": true }`. Four directions to check:
    - `disk_no_catalog.orphans_found` — PDFs on disk with no catalog row. Expected ≈ 0 with the inbox watcher running; non-zero means either the watcher is off or a file just landed and hasn't been swept yet.
    - `catalog_no_markdown.orphans_found` — catalog claims converted but markdown is gone. Expected 0.
    - `catalog_no_source.orphans_found` — phantom catalog rows with neither paper nor markdown. Expected 0 in steady state.
    - `flag_drift.drift_found` — catalog rows whose stage flags disagree with storage (e.g. markdown exists but `conversion == None`, or PDF exists but `downloaded_at == None`). Expected 0 once a prior repair has run.
    Any non-zero count after a recent sweep is a real anomaly worth flagging.

## Phase 3 — Search surface (all 6 providers)

12. `paper_search` for `"retrieval augmented generation"` on each provider individually (`arxiv`, `openalex`, `semantic_scholar`, `pmc`, `crossref`, `core`), `limit=2`, `abstract=true`. Which providers returned results, which errored, latency per call if visible.
13. `paper_search` with filters: `date=">=2024"`, `min_citations=10`, `sort=citations`. Confirm filters applied (look at the `year` and `citations` fields in the results). With the citation-sort relevance floor in place, the top results should be genuinely about RAG — if you see off-topic high-cite papers (ferroptosis, points-of-interest) at the top, that's a regression.
14. `paper_get` by a known-stable DOI (pick one returned above). Full metadata returned?
15. **arXiv DOI resolution** — call both `paper_get("10.48550/arXiv.2005.11401")` and `paper_get("10.48550/arxiv.2312.10997")` (note the case difference). Both should return full metadata (Lewis et al. RAG paper + a follow-up). A `"No paper found"` error on either is a regression in the aggregate `get_by_doi` arXiv shortcut. Then `paper_download("10.48550/arxiv.2312.10997")` should write a PDF > 100 KB — the downloader's case-insensitive fast-path should engage.

## Phase 4 — Ingestion round-trips (mutating — do ONE paper per path)

Pick **one** distill instance and **one** scribe instance for this phase and record which URLs you used.

### 4A — Provider-fetch round-trip

15. `paper_download` for a small open-access PDF (PMC or arXiv DOI from step 13). Record the `stem` / `doc_id` assigned.
16. `scribe_convert` that stem. Time to completion; page count; any OCR warnings.
17. `markdown_read` same stem, first 500 chars — coherent?
18. `distill_index` that stem (force). Chunk count produced. **If zero chunks: this is the main thing to debug — report why** (year filter rejecting? language detection? empty markdown after extraction?).
19. `distill_exists` with the doc_id — returns true?
20. `distill_search` for a distinctive phrase from the markdown. Does the new doc appear in top-5? Score?
20a. `distill_scan_repetitions` with `{ "dry_run": true, "limit": 50, "threshold": 10 }` (lower threshold than the steady-state check so even borderline loops surface). The new doc MUST NOT appear in `flagged` — a fresh convert that trips the scanner means the scribe-side QC gate regressed or the repetition_penalty is too low. Report both the scanner output and a sample of the markdown if flagged.

### 4B — Client-side inbox round-trip (rc.258)

Exercises the `hs scribe inbox` watcher. Only run this if an operator is on the LAN and can drop a file via NFS mount or `aws s3 cp`; otherwise note as not-tested.

21. Drop a small fresh PDF (not already in the corpus) into `papers/manually_downloaded/selftest-<timestamp>.pdf`. Record timestamp and target key.
22. Within 60 seconds, call `catalog_recent` and confirm a `Convert` event appears for the dropped stem. The event's `stem` should match the filename (minus extension).
23. Call `markdown_list` — the target key `papers/se/selftest-<ts>.md` should exist.
24. The source key `papers/manually_downloaded/selftest-<ts>.pdf` should be **gone** (watcher relocated it). If the source is still there after 60s, report as FAIL — the inbox watcher is not running on any client.
25. Repeat the drop with the same filename — on the second drop, the watcher should log `AlreadyAtTarget` and still delete the source. `distill_search` results should not change (no duplicate chunks indexed).

## Phase 5 — Diagnose paths

21. Pick one paper from `catalog_recent` where `markdown: true` but `embedded: false` (if any). Call `distill_index` on it. Capture the failure mode.
22. Pick one paper where `downloaded: true` but `markdown: false` (and `conversion_failed: false`). Call `scribe_convert`. Capture failure mode.
23. Pick one paper where `conversion_failed: true`. Read its catalog row via `catalog_read` and report the `conversion.reason` / `conversion.error` field — that's the rc.253 explanation surface.
24. `distill_search` with `year` filter and `topic` filter combined. Are metadata filters respected?

## Phase 6 — Multi-instance consistency (skip if only 1 of each)

25. If multiple `distill_instances`: call `distill_status` against each and confirm they agree on `collection`, `embed_model`, `qdrant_url`. Divergence here means clients will get inconsistent search results depending on which instance the gateway routed them to.
26. If multiple `scribe_instances`: confirm they agree on `version` and model. A version split mid-rollout is OK (rolling upgrade in progress) — flag it but don't fail.

## Report format

Produce a markdown report with these sections:

- **Verdict** — one line: PASS / FAIL. If FAIL, include the rule(s) that fired (e.g. "CUDA non-negotiable violated on http://big:7434", "pipeline math drift of 17 documents").
- **Topology snapshot** (Phase 0) — backend, gateway-or-direct, instance counts and URLs, Qdrant info, pipeline counts.
- **Health gate results** (Phase 1) — pass/fail per gate with the evidence.
- **Pipeline gap table** — counts at each stage (downloaded → converted → conversion_failed → embedded) and the deltas.
- **Reconciler results** (Phase 2 steps 10–11) — phantom embeds and disk/catalog orphans, with sample stems if any.
- **Per-tool results** — one row per tool call: tool, args (abbreviated), outcome (ok/err), latency, salient field or error message.
- **Round-trip trace** (Phase 4) — the stem with timestamp at each stage; which scribe + distill instance handled it.
- **Failures & hypotheses** — for each error, list 2–3 competing hypotheses with the evidence that points to each. Do not propose fixes.
- **Data quality flags** — empty chunks, low similarity on self-search, broken markdown, missing catalog fields, year drift, stub PDFs.
- **Open questions** — anything ambiguous that needs a human to clarify before debugging further.

Keep the report under ~800 lines. Include exact error strings. Do not sanitize paths, stems, doc_ids, or instance URLs — we need them for grep.

## What this prompt does NOT cover

These need shell access or off-network clients and live in `docs/deployment.md` §9:

- Direct port reachability: 2049 (NFS), 3900/3903 (Garage S3 + admin), 4222 (NATS event bus), 5432 (Postgres), 6334 (Qdrant gRPC), 11434 (Ollama).
- Cloudflare tunnel state and the OAuth enrollment / token-refresh flow (an authenticated client cannot meaningfully test its own auth path).
- NVIDIA driver / CUDA library health beyond what the running services report (`nvidia-smi`, `ldd` of the pyke ort cache, `LD_LIBRARY_PATH`).
- Postgres schema / connectivity — home-still does not yet consume Postgres directly per `docs/deployment.md` §6.4.
