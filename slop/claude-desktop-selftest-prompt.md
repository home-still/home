# Claude Desktop Self-Test Prompt — home-still

Quit and summarize findings at the first error encountered.

Paste this into Claude Desktop (which has the `hs-mcp` server connected). It exercises every tool and returns a debug-focused report.

---

You have access to the `home-still` MCP server (tools prefixed with paper/catalog/markdown/scribe/distill). This is a 211M+ vector academic research pipeline: OpenAlex/arXiv/PMC → PDF download → VLM scribe (markdown) → CUDA distill (Qdrant embeddings) → semantic search.

**Run a full end-to-end self-test and produce a debug report.** Do not skip steps on failure — capture every error verbatim. Prefer small `limit` values; we want signal, not volume.

## Phase 1 — Health & Inventory (read-only)

1. `distill_status` — Qdrant reachable? Collection name, vector count, indexed vs pending, device (cuda/cpu), embed model.
2. `scribe_health` — server up? GPU name + utilization, queue depth, last conversion timestamp.
3. `catalog_recent` with a generous window — last 10 downloads, 10 conversions, 10 embeds. Report timestamps and any gaps (e.g. downloaded but never converted, converted but never embedded).
4. `catalog_list` (page 1, small limit) — total paper count, how many have `markdown: true`, how many `embedded: true`.
5. `markdown_list` (small limit) — sanity check that markdown files exist matching the catalog.

## Phase 2 — Search surface (all 6 providers)

6. `paper_search` for `"retrieval augmented generation"` on each provider individually (arxiv, openalex, semantic_scholar, pmc, crossref, core), `limit=2`, `abstract=true`. Which providers returned results, which errored, latency per call if visible.
7. `paper_search` with filters: `date=">=2024"`, `min_citations=10`, `sort=citations`. Confirm filters applied.
8. `paper_get` by a known-stable DOI (pick one returned above). Full metadata returned?

## Phase 3 — Ingestion round-trip (mutating — do ONE paper only)

9. `paper_download` for a small open-access PDF (PMC or arXiv DOI from step 7). Record the `stem` / `doc_id` assigned.
10. `scribe_convert` that stem. Time to completion; page count; any OCR warnings.
11. `markdown_read` same stem, first 500 chars — is content coherent (not gibberish)?
12. `distill_index` that stem (force). Chunk count produced. If zero chunks: this is the main thing to debug — report why.
13. `distill_exists` with the doc_id — returns true?
14. `distill_search` for a distinctive phrase from the markdown. Does the new doc appear in top-5? Score?

## Phase 4 — Diagnose paths

15. Pick one paper from `catalog_recent` where `markdown: true` but `embedded: false` (if any). Call `distill_index` on it. Capture the failure mode.
16. Pick one paper where `downloaded: true` but `markdown: false`. Call `scribe_convert`. Capture failure mode.
17. `distill_search` with `year` filter and `topic` filter combined. Are metadata filters respected?

## Report format

Produce a markdown report with these sections:

- **Environment snapshot** — Qdrant vectors, device, GPU, service versions, collection name.
- **Pipeline gap table** — counts at each stage (downloaded → converted → embedded) and deltas.
- **Per-tool results** — one row per tool call: tool, args (abbreviated), outcome (ok/err), latency, salient field or error message.
- **Round-trip trace** — the stem from Phase 3 with timestamp at each stage.
- **Failures & hypotheses** — for each error, list 2–3 competing hypotheses with the evidence that points to each. Do not propose fixes.
- **Data quality flags** — empty chunks, low similarity on self-search, broken markdown, missing catalog fields, stamp drift.
- **Open questions** — anything ambiguous that needs a human to clarify before debugging further.

Keep the report under ~800 lines. Include exact error strings. Do not sanitize paths or stems — we need them for grep.
