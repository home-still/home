# home-still improvement loop

Each iteration: find the biggest user problem, research it, implement it
within the project's rules, verify it with numbers, release it, and log it
here. One change per iteration.

---

## 2026-10-20-01 — hybrid (dense + keyword) retrieval

**Change.** `hs distill search` now runs two channels over the same
collection and fuses their **ranks** with Reciprocal Rank Fusion:

- **dense** — the BGE-M3 vector of the query, cosine similarity (unchanged);
- **lexical** — a Qdrant text index over the chunk text (new).

The lexical channel decides which terms to require by how rare they are in
the corpus, because Qdrant's text match is an AND and a real question is
not an AND: "Hashimoto thyroiditis euthyroid" matches no single chunk,
though the passage is in the library. The two rarest terms are required,
falling back to one.

A text index on `chunk_text` is now one of the collection's required
indexes, so the server creates it at startup like every other one.

**Why this ranked first.** The baseline answered 5 of 25 fixed queries
wrongly — not with an error, but with a confident, plausible passage
about the wrong thing. A user asking "what does this corpus say about
Kandel's?" got a paper that cites Kandel in a reference list. That is
the worst failure mode the tool has: silent and wrong. The runner-up
(reference-list filtering) is a *sub-part* of the same problem and is
quantified below; fixing retrieval first is what makes it measurable.

**Evidence.**

| | baseline (dense only) | hybrid |
|---|---|---|
| RECALL@10 | 20/25 = 80.0 % | **25/25 = 100.0 %** |
| concept | 8/8 = 100 % | 8/8 = 100 % |
| literal | 7/10 = 70 % | 10/10 = 100 % |
| named | 5/7 = 71.4 % | 7/7 = 100 % |
| median rank of the expected passage | 1 | 1 |
| median latency | 52 ms | 92–101 ms |
| reference-like chunks in the top 10 | 16.8 % | 17.6 % |

No query that answered before answers differently now: the five the
baseline missed (q09, q10, q12, q19, q22) now answer at rank 1–2, and
the concept group — the one dense retrieval was already good at — is
unchanged. The lexical channel **on its own** also reaches 25/25, which
is the evidence that the two channels are complementary rather than
redundant.

The reference-list noise is pre-existing (the dense channel produces
16.8 % of it alone) and the hybrid raises it by 0.8 points while lifting
recall by 20. Measured in `docs/loop/runs/2026-10-20-01/eval.md`.

**Where the numbers come from.** `docs/loop/queries.yaml` (25 queries in
three groups) and `docs/loop/eval_queries.py`, run against the live
server on `big` over the 438,863-chunk corpus. The harness talks HTTP
directly, so it measures the server rather than the CLI's presentation.

**Research.** `docs/loop/runs/2026-10-20-01/research.md` — Cormack et
al. 2009 (RRF) and the sparse-dense complementarity paper are both *in
this corpus* and were read there; the fusion and the sparse-channel
evidence come from our own `myelin` crate, which already measures
hybrid vs dense-only (0.8666 vs 0.7314) and picks `k = 1` for RRF.

**Version.** `v0.0.1-rc.365` — a patch-level bump. The change adds a
retrieval channel and does not alter any wire format, command surface,
collection schema, or stored data; nothing existing is invalidated and
no re-embed is needed.

**Release.** `v0.0.1-rc.365`, published at
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.365>: 16
platform binaries (5 targets × the `hs` CLI, plus `hs-gateway`,
`hs-mcp`, `hs-scribe-server` and `hs-distill-server` where the target
builds them), 16 `.sha256` files and a combined `checksums-sha256.txt`,
plus the `ghcr.io/home-still/hs-scribe-server` images for both
architectures. The CI run was green on all three gate platforms
(macOS 14, Ubuntu 24.04, Windows 2022) and all five build targets. The
published `hs-distill-server` binary was downloaded and checked: it
carries the `0.0.1-rc.365` version marker and the new lexical-search
code path.

**Commit.** `8423777` — "feat(distill): hybrid dense + keyword retrieval,
fused with RRF".

**Also in this iteration:**

- `docs/loop/needs-decision/async-nats-missed-idle-heartbeat.md` — the
  watcher crash-loop. It is an upstream `async-nats` race, not a
  home-still bug, and the brief says a change to a shared crate is not
  this loop's to make. Written up with three options and a
  recommendation instead of patched.
- The README's known drift (OCR backends, the `openalex-ingest` crate
  and the `personal` feature missing from it) is **not** fixed here: it
  is a documentation change, not a code change, and the rule is one
  change per iteration. It is the first item below.

---

## 2026-10-20-02 — reference lists demoted in search results

**Change.** A chunk that is a bibliography entry is now pushed below the
passages that discuss the topic. It is not dropped: it keeps its score
and stays reachable, so a user asking for a paper's bibliography still
gets it. A page with no prose at all still leads with the reference
list, because sometimes the only chunk in the corpus that mentions a
term is a citation of it.

The detection is structural (`crates/hs-distill/src/reference_list.rs`):
a citation line carries a `doi:`, a parenthesised year after author
names, `et al.`, a page range, a volume, or a numbered-list marker. It
is deliberately conservative — a chunk that mixes a citation with real
prose discusses the topic and cites it, and is not demoted.

**Why this ranked first.** The previous iteration took recall@10 to
100 %, but recall only says the expected string is *somewhere* in the
top 10. Looking at what a user actually sees, a third of the answers
were bibliographies: for "Gazzaniga split brain" the best-ranked hit was
a chunk of `doi:` lines. That is the same class of failure as a wrong
answer — silently wrong, and a user cannot tell from the result that
they got a bibliography instead of a passage. It beat the runner-up
(cross-encoder reranking) because it needs no GPU — the 3090 on `big` is
shared with another agent's job — and it reuses knowledge the project
already has but loses: `hs-scribe` detects bibliography *pages* with
the layout model's classes and uses that to relax its QC budget, and the
verdict never reaches the distill side.

**Evidence.** Measured on the live server over the 438,863-chunk
corpus, the same 25-query set, before and after:

| | rc.365 (before) | after |
|---|---|---|
| RECALL@10 | 24/25 = 96 % | **24/25 = 96 %** (unchanged) |
| reference-like in the top 3 | 26/75 = 35 % | **7/75 = 9 %** |
| reference-like in the top 10 | 81/250 = 32 % | **18/250 = 7 %** |
| median latency | 121 ms | 130 ms |

**Recall is unchanged**, which was the constraint: the demotion reorders
the page, it never changes which hits are in it.

**The rule took three attempts, and the live server caught two wrong
ones.** "Demote every reference list past every prose chunk" is a sort,
and it lost the expected passage for two queries (25/25 → 23/25) because
for those the expected passage was itself a reference list. "Demote only
past prose that outranks the reference list" is too weak — the
reference list usually *does* outrank the prose, so the demotion did
nothing (the reference share in the top 3 went 18/75 → 28/75, up). The
rule that works is "prose exists → prose leads".

The detector also missed two real formats in this corpus until the live
server showed it: markdown bulleted bibliographies with no year at all,
and author lists with the year 103 characters in. Both are now covered
by tests built from real corpus lines. A detector validated only on
synthetic examples would have shipped broken.

**Also corrected:** two query-set expectations that named terms the
corpus only ever cites — "Dawson" here is the botanist T. E. Dawson
(137 chunks, all reference lists) and "Lorenz" likewise. The set now
asks for terms the corpus actually holds.

**Research.** `docs/loop/runs/2026-10-20-02/research.md` — the
techniques considered (cross-encoder rerank, re-index with a
bibliography flag, a structural detector), why the detector was chosen
(needs no GPU, no re-embed), and the threshold measurement that fixed it
at 0.5 of a chunk's lines (a threshold of 0.2 would destroy the 14 % of
chunks that mix a citation with real prose).

**Version.** `v0.0.1-rc.366` — a patch-level bump. It changes the
order of results, not which results, and touches no wire format, command
surface, collection schema, or stored data.

**Commit.** `0fd9ec0` — "feat(distill): demote reference-list chunks
below the passages that discuss the topic".

### Top 5 remaining items

1. **Links to the documents, and highlighting (issue #7).** The wishlist
   item, and it is fully designed: github.com/home-still/home#7 has the
   agreed design for a `hs serve docs` service that serves the source
   PDF/HTML/EPUB and the markdown over signed, expiring URLs, LAN and
   remote, with range requests so a PDF viewer can seek. The pieces it
   needs that home-still does not have: `Storage::get_range` /
   `get_stream` (the trait has `get`, `put`, `head`, `list`, `delete`
   only, and no presign), a docs service, a gateway route, and link
   minting in hs-mcp. The MCP hits already carry `pdf_path`,
   `line_start`, `line_end` and `page` — everything a link and a
   highlight need — so the data is there; what is missing is the
   serving. It depends on #5 below (source scopes), which is not
   started.
   Best carved slice: `Storage::get_range` + `get_stream` with the
   LocalFs and S3 implementations, which is a prerequisite for
   everything else and is independent of the rest.
2. **Cross-encoder reranking.** The remaining 7 % reference-list noise
   in the top 3 is the clearest remaining wrongness, and a cross-encoder
   is what fixes it — it scores a query *against* a passage and can
   tell a bibliography entry from a discussion. Our own `myelin` crate
   already runs `bge-reranker-v2-m3` for exactly this, so the technique
   is proven and the code is ours. Blocked on a second GPU service, and
   on the 3090 being free.
3. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Source scopes (#4), the base for the docs links.** The MCP layer
   has no notion of which source a document came from (academic,
   personal, internet, synthesis, iMessages) and no per-source scope, so
   every tool returns every document. It is the prerequisite for #1's
   personal-document links and their short TTL, and it is what stops a
   personal document leaking through a general search.
4. **Fold `myelin` in — decide, then do it in slices.**
   `myelin-core` (github.com/home-still/myelin, MIT, "Copyright (c) 2026
   Home Still") already has hybrid retrieval, RRF and a reranker, and it
   is *ours*. Measured on 2026-10-20: it is a ~20k-line crate with 46
   source files that depends on `sqlx`, `petgraph` and `schemars` —
   dependencies home-still does not have and, for a document library,
   mostly does not want. Two things it has that home-still does not:
   a BM25 channel implemented as Qdrant **sparse vectors** (better than
   this iteration's payload text index, but it needs every point to
   carry a sparse vector — a full re-embed of 438,863 chunks), and a
   cross-encoder reranker as a separate service.

   The decision to make: does home-still depend on `myelin-core` (and
   drop its own fusion), or do the two stay separate products that share
   techniques and measurements? The honest answer today is **the
   latter**, for one concrete reason: `myelin-core`'s retrieval is built
   for agent memory (multi-tenant, scoped, token-budgeted, with a
   graph channel and a write path), while home-still's is a document
   library (single-tenant, page-cited, read-mostly). Pulling in a
   memory engine to serve a book corpus is a step backwards.

   The parts worth taking, in order of value per unit of coupling:
   the cross-encoder rerank stage (item 2 above) as a *process* to copy,
   and `myelin_core::pipeline::fuse::rrf` as ~15 lines of code if a
   shared crate ever makes sense. Both are cheap because both are
   small; the crate as a whole is not.
5. **Score normalisation across the two channels.** The fused score is
   an RRF score, so it is comparable between hits of one search and
   meaningless across searches. The CLI prints it as `[score: 0.500]`,
   which reads like a cosine. A user comparing two searches cannot
   compare the numbers; the API docs say so, but the CLI should not
   print a number that invites the wrong comparison.

---

## 2026-10-20-03 — the title gap in search results

**Change.** A document's title is copied from its catalog row into the
Qdrant payload at index time, and a search hit reads it back out of the
payload. A document indexed *before* its catalog row existed was
therefore indexed with no title, and — because the corpus is not
re-indexed — it stays that way. Two parts:

- **`hs distill backfill-titles`** — a new CLI command (dry-run by
  default) that writes a title onto the points that lack one. The title
  comes from the catalog where the catalog has one, and from the
  document's own text where it does not. It is a **payload update**: the
  vector is untouched, so it is not a re-embed and needs no GPU.
- **The indexer no longer produces a title-less point.** A document with
  no catalog row takes its title from its own text — its first heading,
  *or* its first plain line. The plain-line rule matters: measured over
  150 sampled documents, a heading gives a title 33 % of the time and a
  first plain line 53 %, so a heading-only rule leaves two thirds of the
  corpus without a title.

**Why this ranked first.** Over the 250 hits of the fixed 25-query set,
**72 % of the hits carried no title**: an agent that called
`distill_search` got a passage and a `doc_id` and could not tell the
user *which book* the passage came from. It is the same class of failure
as a wrong answer — silently wrong — and it is the precondition for
issue #7's links, which need a title to be a link *to something*. It
beat the runner-up (cross-encoder reranking) because it needs no GPU —
the 3090 on `big` is shared with another agent's job — and because a
document with no title cannot be matched *by title* at all, which is
exactly the kind of specific query the lexical channel exists to serve.

**Evidence.**

| | before | after |
|---|---|---|
| hits with no title | 180/250 = **72.0 %** | **52/250 = 20.8 %** |
| indexed documents carrying a title | 27.5 % | **82.2 %** |
| RECALL@10 | 24/25 = 96 % | 24/25 = 96 % (unchanged) |
| median latency | 138 ms | 151 ms (unchanged) |

**Recall is unchanged**, which was the constraint: the change writes a
title that was already known, onto a payload. It adds no retrieval
behaviour.

The 2,812 documents that remain untitled have neither a catalog title
nor one their own text yields — their markdown begins with body text,
because the OCR of those PDFs lost the title page. They are reported by
name, never given a placeholder.

**The live corpus caught a defect in the first implementation.** The
first pass wrote 178 titles carrying the converter's markup (`<table>
<tr><br>…`) and 16 that repeated themselves, because the OCR of a PDF
with a table emits the table where the title page should be and the
first-line rule took it. A title shown to a user verbatim, carrying
markup, is worse than no title. Three rules now reject them, each with
a test built from a real corpus line: a line that **begins with a
table** is the converter's furniture, a line that **repeats itself** is
an OCR artifact, and a title that merely carries a tag *inside* it
(`<i>Arabidopsis</i>`, `CO<sub>2</sub>`) is a good title that is merely
marked up. The first version of that last rule flagged 209 titles, of
which 153 were good titles with a tag inside — the rule was wrong, and
the corpus said so. The shipped rule flags **47**, and every one of the
47 is garbage.

The backfill also **corrects** an unusable stored title rather than only
filling a missing one.

**Research.** `docs/loop/runs/2026-10-20-03/research.md` — RocketQAv2
(Ren et al. 2021) and DPR (Chen et al. 2020), both *in this corpus*,
make the same point from the retrieval side: the passage encoding is
`title + body`, so a point indexed without its title is a point the
retriever cannot match by title. The title is not metadata *about* the
document; it is part of what the document is.

**Timings.** `GET /docs/titles` over the whole collection: 8.2 s (one
scroll of 438,863 points). `POST /doc/{id}/title`: 1–21 ms per document.
The full backfill of 5,002 documents: 14 min at 16 concurrent writes,
which lost 207 documents to Qdrant timeouts (the live indexer writes to
the same collection) — 4 concurrent writes with a retry on the write
finished the remainder without a failure.

**Version.** `v0.0.1-rc.367` — a patch-level bump. It changes the
payload of existing points and adds one CLI command; it changes no wire
format, no collection schema, and no vector.

**Commit.** `6a3abe9` — "feat(distill): backfill the missing titles onto the
indexed points".

### Top 5 remaining items

1. **Links to the documents, and highlighting (issue #7).** The wishlist
   item, and it is now closer: the title a link needs is on the payload
   for 82 % of the documents. What is still missing is the *serving*:
   `Storage::get_range` / `get_stream` (the trait has `get`, `put`,
   `head`, `list`, `delete` only, and no presign), a docs service, a
   gateway route, and link minting in hs-mcp. The MCP hits already carry
   `pdf_path`, `line_start`, `line_end` and `page` — everything a link
   and a highlight need — so the data is there; what is missing is the
   serving. It depends on #5 below (source scopes), which is not started.
   Best carved slice: `Storage::get_range` + `get_stream` with the
   LocalFs and S3 implementations, which is a prerequisite for
   everything else and is independent of the rest.
2. **Cross-encoder reranking.** The remaining 7 % reference-list noise
   in the top 3 is the clearest remaining wrongness, and a cross-encoder
   is what fixes it — it scores a query *against* a passage and can
   tell a bibliography entry from a discussion. Our own `myelin` crate
   already runs `bge-reranker-v2-m3` for exactly this, so the technique
   is proven and the code is ours. Blocked on a second GPU service, and
   on the 3090 being free.
3. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Source scopes (#4), the base for the docs links.** The MCP layer
   has no notion of which source a document came from (academic,
   personal, internet, synthesis, iMessages) and no per-source scope, so
   every tool returns every document. It is the prerequisite for #1's
   personal-document links and their short TTL, and it is what stops a
   personal document leaking through a general search.
