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

**Release.** `v0.0.1-rc.367`, published at
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.367>: 33
assets — 16 platform binaries (5 targets × the `hs` CLI, plus
`hs-gateway`, `hs-mcp`, `hs-scribe-server` and `hs-distill-server`
where the target builds them), their `.sha256` files and a combined
`checksums-sha256.txt`, plus the `ghcr.io/home-still/hs-scribe-server`
images for both architectures and the multi-arch manifest. The CI run
was green on all three gate platforms (macOS 14, Ubuntu 24.04, Windows
2022) and all five build targets. The published `hs-distill-server` and
`hs` binaries were downloaded and checked: both carry the
`0.0.1-rc.367` version marker, and the published CLI carries the new
`hs distill backfill-titles` command.

Deployed to the fleet with `hs upgrade --pre --yes`, in series: `big`,
`two`, `bmb`, `mac_air`, `big_mac` — every host reports
`hs 0.0.1-rc.367`, and every server binary on disk carries the rc.367
marker. `big` restarted 7 services, `two` 1 (`hs-gateway`), `bmb` 1
(`com.home-still.scribe`), `mac_air` 1 (`io.home-still.scribe-inbox`).
`big`'s distill server came back on CUDA and `hs distill search` on the
upgraded host returns titles. `five` was offline (unchanged); `one`,
`three` and `four` run no home-still container. `mac_air` still has no
`com.home-still.scribe` agent loaded — the pre-existing state, not a
regression: it is not a scribe pool member.

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

---

## 2026-10-20-04 — signed, expiring links to the documents

**Change.** A docs service that serves the corpus's documents over
signed, expiring links, a CLI that mints them, a gateway route that
proxies to the service, and MCP tools that hand them to agents.

An agent that called `distill_search` got a passage and a `pdf_path`,
and the `pdf_path` was a storage key on the server's disk
(`papers/10/10.3390_s21103461.pdf`) — something no user and no remote
agent can open. The passage was real; the user was one click short of
the source with no way to close the gap. That is the wishlist's first
item, and issue #7.

- **`hs serve docs`** (port 7446) serves `/docs/v1/<stem>/source` and
  `/docs/v1/<stem>/markdown`. The client never names a file: the stem
  is validated and the service probes the same pdf → html → epub order
  the MCP server uses. The bytes come from the same `Storage` trait
  everything else reads through.
- **The signature** is `HMAC-SHA256(key, "v1|stem|fmt|sub|iat|exp")`
  and every field it covers is in the query string, so changing the
  stem, the format, the subject or the expiry invalidates it. The key
  lives in `~/.home-still/docs-link.key`, mode 0600, never
  overwritten.
- **Ranges** are answered: a PDF viewer seeking to a page sends a
  `Range` and gets a 206 with the right `Content-Range`.
- **The gateway** gets a `/docs` route that proxies without a bearer
  token (the signature *is* the authorization) but with the same path
  check, rate limit and concurrency cap as every other proxied path.
  `docs` is routable but not scope-bearing, so `SERVICES` (grantable
  scopes) and `ROUTABLE_SERVICES` (routable services) are now
  separate lists.
- **The links** are on `distill_search`, `abstract_search`,
  `catalog_read` and the new `doc_link` tool, as a LAN URL and a
  remote URL for the source and the markdown. `pdf_path` is **gone**
  from the search hits: it was a storage key presented as if it were a
  link, which it is not.

**Why this ranked first.** Of the 250 hits of the fixed 25-query set,
every one carried a `pdf_path` and not one of them could be opened.
That is the wishlist's own first item, asked for twice by the user
("whenever an agent calls search and pulls text, it also is given
links to documents that the user of the agent can view"), and it is
the precondition for the second half ("highlight the text in the
document"), which needs the document to be servable at all. It beat
the runner-up (cross-encoder reranking) because it needs no GPU — the
3090 on `big` is shared with another agent's job — and because a
document a user cannot open is a document they cannot check the
answer against.

**Evidence.**

| | before | after |
|---|---|---|
| a search hit carries a link the user can open | **no** | **yes** |
| `pdf_path` on a search hit (a storage key, unopenable) | present | **removed** |
| RECALL@10 | 24/25 = 96 % | 24/25 = 96 % (unchanged) |
| median latency | 138 ms | 143 ms (unchanged) |

**Recall is unchanged**, which was the constraint: the change adds a
link to a hit and removes a path from it. It adds no retrieval
behaviour.

**Verified end to end, on real documents**, against the live corpus:

- A real PDF served with every header a viewer needs (`content-type:
  application/pdf`, `content-disposition: inline`, `accept-ranges:
  bytes`, `etag`, `last-modified`, `content-security-policy: sandbox`,
  `x-content-type-options: nosniff`, `referrer-policy: no-referrer`,
  `cache-control: private, max-age=…`).
- A ranged request → `206` with `content-range: bytes 1000-1099/89310`.
- The markdown link → `text/markdown; charset=utf-8`.
- A tampered stem, a tampered expiry, a forged signature, a missing
  signature, an expired link, a link for one format used on the other,
  and a revoked subject → **403**, every one with the same body, so a
  client learns nothing about the corpus.
- A path-traversal stem → **400**, before storage is read.
- **It streams**: the corpus's largest object is a 294 MB PDF, and a
  full download of it grew the service's RSS by **0.4 MB** (51.7 →
  52.1 MB), against an acceptance criterion of under 64 MB.
- The whole chain, end to end: an agent calls `distill_search` over
  MCP, gets a `links` object, and the URL in it fetches the actual
  PDF (`%PDF-1.5`).

**Two bugs the tests caught**, both in the link's own logic: the expiry
comparison was `>` where it had to be `>=` (a link minted with a zero
TTL lived for the rest of that second), and the link carried no issue
time, so a revocation could not be applied to it. The link now carries
`iat`, it is part of the signature, and the revocation is checked
against it.

**Also in this iteration — a release defect that had the scribe server
crash-looping for six hours.** `hs-serve-scribe.service` on `big`
restarted 370 times, each ending in "the execution provider could not
be registered because its corresponding Cargo feature is not enabled".
Two causes, neither visible to a gate: the release workflow built the
scribe server with `--features server` while the distill server used
`--features cuda` — the `cuda` feature is what registers the ONNX CUDA
provider, and it was never enabled in any release — and the scribe
server never got the `LD_LIBRARY_PATH` that the distill server has had
since rc.267, so even with the feature the provider could not find the
CUDA-12 runtime libraries a CUDA-13 host lacks. Both are fixed: the
workflow carries a per-target `scribe_features`, the CI gate compiles
and tests `--features cuda` the way it already does for
`hs-distill --features cuda`, and both services now call one
`hs_common::cuda_libs::cuda_ld_library_path`. The unit is stable with
zero restarts and `/health` reports `layout_model: true`,
`table_model: true` on the 3090.

**Research.** `docs/loop/runs/2026-10-20-04/research.md` — the
techniques considered (a storage key, a `file://` URL, a presigned S3
URL, an unauthenticated browse service, a signed-link service), why
the last one, and why the *signature is checked by the service that
reads the bytes* rather than by the gateway: the gateway already
forwards `Range`, `Content-Range`, `ETag` and `Accept-Ranges`
untouched, so E2 costs almost nothing at the gateway and gives the LAN
and remote URLs the same check with no "trusted LAN" bypass.

**Version.** `v0.0.1-rc.372` — a patch-level bump. It adds a service
and a command, changes no wire format, no collection schema and no
stored data, and no vector is recomputed.

**Three release candidates were lost to the Windows gate**, and all three
were real defects in the new code, not CI noise:

- **rc.368** failed on a `use std::os::unix::fs::OpenOptionsExt` that
  was outside the `#[cfg(unix)]` guarding the `mode(0o600)` call that
  needs it. A `#[cfg(unix)]` on the *call* is not a `#[cfg(unix)]` on
  the *import*.
- **rc.369** failed a *test*: the loader path was joined with `:`,
  which on Windows makes the whole path one malformed entry and every
  library in it unfound. The separator is `;` on Windows and `:` on
  Unix, and a test now asserts it is the platform's.
- **rc.370** failed the *same* test, for the mirror reason: the test
  compared against a hard-coded `:`. The path was right and the test
  was wrong. The tests now split on the platform's separator, the same
  one the code joins with.

Tags are immutable, so each fix went into the next rc rather than
into a retagged one. The lesson is the same both times: only a
platform you do not develop on can tell you that a path is a path.

**rc.371 shipped and was deployed**: all three gates green, 33 assets,
and the published `hs-scribe-server` carries the CUDA provider — the
scribe server on `big` came back with `layout_model: true` and
`table_model: true` and zero restarts, from the *released* artifact
rather than a hand-built one. Every host is on rc.371.

**rc.372** adds `hs serve docs --install-user`: the docs service is a
service the operator runs on their own machine, and `--install` writes
a systemd unit under /etc, which needs root. The user-service form
needs no sudo, and `--uninstall` removes both forms. Two `conflicts_with`
attributes on one argument means the second replaces the first, so the
conflicts are declared with `conflicts_with_all`.

**Release.** `v0.0.1-rc.372`, published at
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.372>: 33
assets, the same five build targets and both Docker images. The
published `hs` binary was downloaded and checked: it carries the
`0.0.1-rc.372` marker and the `hs serve docs --install-user` flag.

One build failed on the way there — `aarch64-apple-darwin`, on a
GitHub **artifact-upload** timeout, after the build itself had
succeeded ("Finished `release` profile in 6m 15s"). That is CI
infrastructure, not the code: the job was re-run and passed. The
distinction matters, and the log says which it was.

**Deployed** with `hs upgrade --pre --yes`, in series: `big`, `two`,
`bmb`, `mac_air`, `big_mac` — every host reports `hs 0.0.1-rc.372`.
`big` restarted 8 services, the others 1 each. The scribe server on
`big` came back from the *released* artifact with `layout_model: true`
and `table_model: true` and **zero restarts** — the crash-loop is gone
from the release, not just from a hand-built binary. The docs service
runs as `hs-serve-docs.service` under the user's systemd, answering
on :7446, and a link minted with the released CLI opens a real PDF.

### Top 5 remaining items

1. **Highlight the text in the document (the wishlist's second half).**
   The links exist; the anchor does not. The MCP hits already carry
   `line_start`, `line_end` and `page`, so the data is there — what is
   missing is a way to address a span in the served document: a
   `#page=11` fragment for a PDF, and a text-fragment selector for the
   markdown. The docs service serves a range today; a `page=N` query
   on the markdown route is the smaller half and is not started.
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
5. **Source scopes (#4).** The MCP layer has no notion of which source
   a document came from (academic, personal, internet, synthesis,
   iMessages) and no per-source scope, so every tool returns every
   document. It is the prerequisite for personal-document links with
   their short TTL, and it is what stops a personal document leaking
   through a general search.

---

## 2026-10-21-05 — the link points at the passage

**Change.** A document link can now name **the page** the passage is
on, and the passage itself.

- The **PDF** link carries `#page=N`. Every PDF viewer honours it and
  scrolls to that page.
- The **markdown** link carries a W3C **Text Fragment**,
  `#:~:text=<the passage>`. The browser finds the quoted text in the
  document it loaded, scrolls to it and selects it. It is resolved in
  the *client*, against the bytes the server sent — the server serves
  the document, unchanged.
- The **markdown route** takes `?page=N` and serves that one page.

The page is **signed**: it is part of the request, so a link minted for
page 11 opens page 11 and nothing else, and a tampered page is a 403
exactly like a tampered stem.

**Why this ranked first.** The previous iteration gave an agent's hit
a link the user can open — at the *top* of the document. The user's
question is "show me *this* passage", and the answer was a PDF at page
1 with three thousand pages of paper around it. This is the
wishlist's second half, stated in one line: *"a really cool feature
would be able to highlight the text in the document when all this needs
to be served remotely."* It beat the runner-up (cross-encoder
reranking) because the anchor data was **already in the corpus** — 90 %
of hits carry a page number — so the whole change is a URL and a
signature field, with no re-index and no GPU.

**Evidence.**

| | before | after |
|---|---|---|
| a search hit's link points at the passage | **no** | **yes** |
| the PDF link names the page | no | **yes** (`#page=N`) |
| the markdown link quotes the passage | no | **yes** (`#:~:text=…`) |
| the markdown link serves one page | no | **yes** (`?page=N`) |
| RECALL@10 | 24/25 = 96 % | 24/25 = 96 % (unchanged) |
| median latency | 158 ms | 174 ms (unchanged) |

**Recall is unchanged**: the change adds a URL to a link and a page to
a route. It touches no retrieval behaviour and no stored data.

**Verified end to end, on real documents.** The markdown of
`10.1677_joe.1.06233` is 73,923 bytes over 17 pages; the link that
names page 15 serves 7,453 bytes, starting with the reference list,
and answers `content-location: #page=15`. The browser's match works:
**for every sampled hit, the quoted snippet is a substring of the page
its link serves** (84–110 characters, three of three).

**Two things measured rather than assumed.**

- **How long a snippet should be.** The first implementation quoted the
  passage's whole first line: over the corpus's own passages that
  averages **308 characters** and reaches **878** — too long for a URL
  a user has to read. The snippet is now cut at a **word boundary** at
  120 characters, which keeps it a substring of the document (a
  snippet cut mid-word is not, and the browser would find nothing).
- **Whether the markdown's pages are the PDF's pages.** They are, and
  the code says so: the scribe renders the PDF's pages in order, sorts
  the concurrent results back by page index, and joins them with a
  **reserved** separator, so markdown page N *is* source page N.
  Verified on a real 17-page document: 17 markdown pages, 17 PDF pages,
  and the markdown's page 15 is on the PDF's page 15. The one case
  where it does not hold is a single-page markdown for a multi-page
  source (the olmocr backend's flat output), where the catalog row
  records no page offsets and the chunk has no page — the link is
  still correct, it just names the whole document, because there is
  nothing better to point at.

**Research.** `docs/loop/runs/2026-10-21-05/research.md` — the
techniques considered (a `#page=N` fragment, a text fragment, a
server-side `<mark>` injection, all three), and why the *client* does
the highlighting: a server that rewrites the document it serves
cannot also be the thing an agent reads, and the highlight belongs
where the browser can resolve it.

**Version.** `v0.0.1-rc.373` — a patch-level bump. The change adds a
query parameter and a URL fragment: it changes no wire format's shape,
no collection schema and no stored data, and no vector is recomputed.

**Release.** `v0.0.1-rc.373`, published at
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.373>: 33
assets, the same five build targets and both Docker images. The
published `hs` binary was downloaded and checked: it carries the
`0.0.1-rc.373` marker and the `hs docs link --page` flag.

**Deployed** with `hs upgrade --pre --yes`, in series: `big`, `two`,
`bmb`, `mac_air`, `big_mac` — every host reports `hs 0.0.1-rc.373`.
`big` restarted 9 services, including `hs-serve-docs`. Verified on
the deployed release: a link naming page 15 of
`10.1677_joe.1.06233` serves that page (7,453 bytes of a 73,923-byte
document, starting with the reference list) and answers
`content-location: #page=15`; the PDF link answers the same; and a
link whose page was edited from 15 to 3 is a **403**.

### Top 5 remaining items

1. **Cross-encoder reranking.** The remaining 7 % reference-list noise
   in the top 3 is the clearest remaining wrongness, and a cross-encoder
   is what fixes it — it scores a query *against* a passage and can
   tell a bibliography entry from a discussion. Our own `myelin` crate
   already runs `bge-reranker-v2-m3` for exactly this, so the technique
   is proven and the code is ours. Blocked on a second GPU service, and
   on the 3090 being free.
2. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting.
3. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
4. **Source scopes (#4).** The MCP layer has no notion of which source
   a document came from (academic, personal, internet, synthesis,
   iMessages) and no per-source scope, so every tool returns every
   document. It is what stops a personal document leaking through a
   general search, and it is what would give a personal document a
   shorter link TTL.
5. **The `personal` store behind `Storage`.** The personal store sits
   on the local filesystem outside `Storage`, so the docs service
   cannot serve a personal document — which is also why issue #7's
   "personal documents get links" is not started. Moving it behind
   `Storage` (a `hs migrate personal-to-storage`) would let one read
   path serve every document the corpus holds.
## 2026-10-21-06 — the demoter that cannot see the corpus's bibliographies

**Change.** The search path already demoted reference lists below the
passages that discuss the topic (rc.366). The demoter was wired in and
correct — but its **detector could not see the bibliographies this
corpus actually contains**, so 7 of the 25 fixed queries opened with a
bibliography and the user got a list of citations instead of a passage.

The detector now recognises the four styles the corpus really uses:
a **bare year** after an author list (`Sundralingham, P. 1983 …`), an
**ALL-CAPS APA** list (`ALBERG, D., LAST, M., AND KANDEL, A. 2012. …`),
a **full given name** (`Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua
Bengio. 2015. …`), and a **reference number other than 1 or 2**
(`59. Ferguson JN, …`, `(34) Lewis, J. A. …`).

**Why this ranked first.** It beat the runner-up (cross-encoder
reranking) for three reasons, all measured:

1. **The noise is the largest remaining wrongness.** The cross-encoder
   is the right long-term answer, but it is *blocked*: it needs a second
   GPU service and the 3090 on `big` is shared — at the time of
   measurement it had 22.4 of 24.6 GB used, with a 12 GB VLLM tenant on
   it. This fix needs no GPU at all.
2. **It is a bug, not a tuning problem.** The demoter exists and is
   correct; the rule set is wrong. A user asking a question gets a
   bibliography for 28 % of the fixed query set, and the passage that
   answers the question is *in the results* — behind it. Nothing is
   missing from the corpus and nothing needs re-indexing.
3. **It is one file.** The whole change is
   `crates/hs-distill/src/reference_list.rs` plus its tests. The
   cross-encoder is a new service, a new model on disk, a new latency
   budget, and a new failure mode.

**The evidence.** Measured on the live corpus, 2026-10-21, **A/B on the
same corpus and the same query set**: the running rc.373 server on
`:7434` and the new rc.374 server on `:7436`, both reading the same
Qdrant collection, both on CUDA. The judge is **independent** — the
eval harness carries its own detector, so the metric does not depend on
the code under test:

| | rc.373 (before) | rc.374 (after) |
|---|---|---|
| the **first** hit is a reference list | **7/25 = 28 %** | **1/25 = 4 %** |
| reference-like hits in the top 3 | 18/75 = 24 % | **3/75 = 4 %** |
| reference-list share of the page | 54/250 = 22 % | **30/250 = 12 %** |
| RECALL@10 | 24/25 = 96 % | **25/25 = 100 %** |
| median latency | 149 ms | 150 ms |

**Recall went up, not down** — from 96 % to 100 %. The constraint was
"must not drop"; it improved, and the reason is the same mechanism as
the fix: for `q25` ("Rosalind Franklin and the diffraction data of
DNA") the expected passage was at rank 11 of the page, pushed out of
the top 10 by a bibliography that outranked it. Demoting the
bibliography promoted the passage back into the page — it now sits at
rank 9. The six queries whose first hit changed all changed the same
way: from a bibliography to prose.

**The one query that still opens with a bibliography** is `q14`
("hippocampal neurogenesis and adult memory"), and it is a **mixed
chunk** — a citation list with a line of prose attached, at a share
just under 0.5. The threshold is deliberately conservative: a chunk
that mixes a citation with real prose discusses the topic and cites it,
and demoting it would hide the prose. The one remaining case is the
right trade.

**End to end on the live corpus, with timings.** The same three
queries through both servers:

| query | rc.373's first hit | rc.374's first hit |
|---|---|---|
| "oxytocin social behaviour and vasopressin" | a 12-entry bibliography of neuroscience citations (1,890 ms) | *Human endogenous oxytocin and its neural correlates show adaptive responses to social touch…* (1,263 ms) |
| "nutrient limitation of tropical forest growth" | a bibliography (285 ms) | the paper that discusses it (199 ms) |
| "3D printing of controlled release pharmaceutical bilayer tablets" | a table chunk (352 ms) | the same table chunk (300 ms) — **a table is not a bibliography, and it is not demoted** |

**The root cause, read off the code.** `is_citation_line` is
*disjunctive*: a `doi:`, an `et al.`, a parenthesised year, a `pp.`, a
`vol.`, a `[n]`, a bare URL, a retrieval phrase, or a bulleted author
list makes a line a citation. That set covers the formats the rule was
written against. It misses the corpus's, because it looks for a
**parenthesised** year and the corpus writes a **bare** one, and because
`is_numbered_entry` only strips `1.` and `2.` while the corpus numbers
its references into the hundreds.

The new rules are **conjunctive** — an author list **and** a year, or a
reference number **and** an author — which is what lets them be
recognisable without catching prose. A prose sentence has a year, or a
number, but it does not have a capitalised surname followed by initials.

**Every crossing was inspected.** The new rules can only *add*
detections, so the precision question is: of the chunks they newly call
reference lists, how many are not bibliographies? Measured on a
**761-chunk** pool (every distinct chunk the fixed query set returns at
a deep limit, 50 per query), the new rules call **91 (12.0 %)** a
reference list, and **all 91 were read by eye: every one is a genuine
bibliography**. Zero false positives. The one that looked like prose at
first — a paper's conclusion followed by its `References` section — is a
chunk that is *mostly* bibliography, and the 0.5 threshold is what
decides that correctly.

**The three bugs the tests caught.** The first implementation of the new
rules was wrong three times, and the tests found each:

* `has_author_list_shape` split the line on commas, so `Sundralingham,
  P.` — a surname and an initial separated by a comma — never matched.
  The scan is over **author pairs**, not whitespace-separated tokens.
* `has_standalone_bare_year` claimed `Genes Dev. 2009, 23, 939-950`,
  which is a journal, a volume and a page range, not a bare year.
* the initials check accepted only *one* initial, so `Tinetti ME` — two
  — did not match.

**The literature, and the corpus's own prior art.** Searched the corpus
first. `10.1007_s10648-025-10073-9` describes its own preprocessing:
*"we then removed the abstract and references from the output using
regular expressions"* — rule-based reference removal is the published
practice, not a heuristic invented here. *A Hybrid Approach and Unified
Framework for Bibliographic Reference Extraction* (arXiv 1912.07266)
states the standard heuristic and its limit directly: *"adopted
heuristics will look for comma-separated author names at the start of
the reference string. The drawback … is that it will not be able to
detect a reference if it does not comply with defined heuristics …
Every domain has its unique referencing style and sometimes multiple
referencing styles exist within one domain, making simple approaches
unsuitable for this complex task."* Two things follow, and they are the
design: the signal is the **author list**, not the punctuation, and a
**rule set** is needed because the styles coexist.

**Research.**
`docs/loop/runs/2026-10-21-06/research.md` — the problem measured,
the root cause, the literature, the technique chosen, and the expected
effect. `docs/loop/runs/2026-10-21-06/eval.md` — the before/after
numbers, the precision measurement, the three bugs the tests caught,
and how the numbers were produced.

**What this is not.** Not a re-index: no vector is recomputed, no chunk
is re-cut, the demotion happens at query time. Not a corpus change:
nothing is rebuilt, purged or re-embedded. Not a cross-encoder: that
remains the right long-term answer and stays the first item of the
backlog; this is the 80 % that needs no GPU.

**Version.** `v0.0.1-rc.374` — a patch-level bump. It changes the
*order* of results, not which results, and touches no wire format,
command surface, collection schema or stored data. A patch bump, like
rc.366 — the same kind of change: a search result's *position*, not its
presence.

**Commit.** `5bad869` — "feat(distill): the demoter can see the corpus's
bibliographies".

**Release.** `v0.0.1-rc.374`, tagged from `3e45f18` on `main`:
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.374> — 33
assets, the same five build targets and both Docker images, all twelve
jobs green. The published `hs` binary was downloaded and checked: it
reports `hs 0.0.1-rc.374`, carries the `0.0.1-rc.374` version marker
and is x86-64; the published `hs-distill-server` — the binary that
carries the fix — carries the same marker.

**Not deployed.** The live services on `big` were not upgraded: this
iteration's rules say not to run `hs upgrade` on live services. The
A/B was run instead with the new binary on an alternate port against
the same Qdrant, with the live service on `:7434` untouched.

### Top 5 remaining items

1. **Cross-encoder reranking.** Still the right long-term answer for
   the rest of the ranking wrongness — and still **blocked**: it needs a
   second GPU service, and the 3090 on `big` is shared. Measured
   2026-10-21 at 22.4 of 24.6 GB used, with a 12 GB VLLM tenant on it
   for part of the day. The reference-list noise is now 4 % of the
   first hits; what is left is the *order* of the prose.
2. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.
3. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
4. **Source scopes (#4).** The MCP layer has no notion of which source
   a document came from (academic, personal, internet, synthesis,
   iMessages) and no per-source scope, so every tool returns every
   document. It is what stops a personal document leaking through a
   general search, and it is what would give a personal document a
   shorter link TTL.
5. **The `personal` store behind `Storage`.** The personal store sits
   on the local filesystem outside `Storage`, so the docs service
   cannot serve a personal document — which is also why issue #7's
   "personal documents get links" is not started. Moving it behind
   `Storage` (a `hs migrate personal-to-storage`) would let one read
   path serve every document the corpus holds.
## 2026-10-21-07 — the status that said "not running" about a running server

**Change.** `hs distill status` reported the distill server's liveness
from a **PID file**. The live server on `big` runs under **systemd**, so
no PID file is written for it — and the file that was there is from
**April 14**, naming a PID that no longer exists. The command therefore
reported a healthy, serving server as **dead**, and contradicted itself
one line later.

The liveness signal is now the server's **own answer**: the status
command pings `/health` — the request it was already making, one line
later, to fetch the collection and the point count — and that answer
is the verdict. The server now also reports **its own PID** in
`/health`, so the report can name the process without the client
guessing it: the server is the only party that knows its PID, whether
systemd, launchd or a hand-typed command started it.

**Why this ranked first.** It beat the runner-up — **cross-encoder
reranking**, the backlog's first item, whose GPU blocker has just
cleared (18 GB of 24.6 GB free on `big`) — on two counts. The
reranker is **not in home-still at all**: no code path, no client, no
service, no endpoint; the model is on disk but nothing serves it.
Building it is a new GPU service, a new serving path and a new latency
budget, on a GPU another agent's job took 12 GB of earlier today.
And the status wrongness is a **definite bug** rather than a tuning
problem: a command that operators use in the deployment runbook's own
verification step tells them a healthy server is down, and the wrong
answer is worse than no answer, because it sends them to restart a
server that is serving.

**The evidence.** Measured on the live server on `big`, 2026-10-21,
reading the real 445,451-point collection:

```
$ hs distill status                          # before
Qdrant: OK (http://localhost:6333)
Server: not running          <-- WRONG
Collection: academic_papers
Points: 445451               <-- fetched FROM that server, one line later
Device: Cuda
```

The live server was real: PID 2227527,
`/home/ladvien/.local/bin/hs-distill-server`, started by
`hs-serve-distill.service`, answering `/health` with `{"status":"ok"}`.

| | before | after |
|---|---|---|
| the live server's liveness | **"not running"** (wrong) | **"running"** |
| the PID reported | none (a stale file was read) | **the server's own PID** |
| a dead server's report | "not running", no address | "not running", **with the address and the reason** |
| the report contradicts itself | **yes** | **no** |
| the command's median duration | 372 ms (through ssh) | 392 ms |

**The project already had the rule.** `CLAUDE.md`: *"Watcher liveness is
heartbeat-based, not PID-file-based. When diagnosing 'is the watcher
alive?' check `last_tick_seconds_ago` from status, not `/var/run/*.pid`."*
The rule was written for the watchers; the distill server's *status
command* used the pattern the rule forbids, with the same consequence.
And `hs status` — the main dashboard — already probes each service's
health endpoint and already reported the distill server correctly. It
was only the per-service command that was wrong, which is what made the
inconsistency so easy to miss.

**The technique, from the corpus.** Searched home-still on itself first.
*Software Architecture in Practice*, 4th edition (in the corpus), in
the section on system monitoring, names the tactic: *"**Ping/echo.** An
asynchronous request/response message pair is exchanged between nodes;
it is used to determine reachability… In addition, the echo indicates
that the pinged component is alive. The ping is often sent by a system
monitor."* And on the watchdog variant: *"the process being monitored
will periodically reset the watchdog counter/timer as part of its signal
that it's working correctly."* The stale PID file is a watchdog that
was never reset, because the process that would have reset it is not
the one running.

**The signal is one path.** There is no "if the PID file says X and
the health check says Y, prefer Y" — the health check *is* the
verdict, because it is the only signal that is about the service rather
than about a file. The PID file is not consulted for liveness at all.

**The wire format grows by one optional field.** `HealthResponse` gains
`pid: u32` with `#[serde(default)]`, so a new client reads an old
server's health and an old client reads a new server's health. The
`/health` contract is otherwise untouched, and `/health` is
unauthenticated — it says *that* a dependency is down, never why.

**Verified end to end, on the live corpus.** The new CLI against the
live server reports `running`; against the new server (which reports
its own PID) it reports `running (PID 2257468)`, and that PID was
checked against the process table. The failure path names the address
and the reason: *"not running (no answer at http://127.0.0.1:1/health:
… Connection refused)"*. The search path is untouched — the same new
server served searches while the status was taken.

**The query set is unchanged by this change**, as it must be — it adds
a field to a health response and changes what a status command reports.
Re-run on the new server (which carries rc.374's demoter fix as well):
RECALL@10 **25/25 = 100 %**, the first hit a reference list in 1/25, the
same as rc.374 measured. On the still-deployed rc.373 server the same
query set gives 24/25 and 7/25 — the difference is rc.374, not this
change.

**The tests.** Three, in `crates/hs/src/distill_cmd.rs::status_tests`,
on the project's `FakeServer`: a server that answers is reported as
running and its PID is shown; a server that does not answer is reported
as down *and the report says where it looked*; and **the report does
not contradict itself** — that third one asserts the *invariant* the
old output violated, not the shape of the new output, so it is the test
that would have caught this bug.

**CI caught a flaw in the first version of that test**, on all three
platforms at once: it asserted that a live server produced *no* error,
but the status command also probes Qdrant, which is not running in CI —
so the test was asserting about the environment as well as about the
code under test. The assertion now checks what the test is about — that
the report does not say the server is down — and nothing else. Verified
with Qdrant deliberately unreachable
(`HS_DISTILL_QDRANT_URL=http://localhost:6399`), which is CI's
condition: all three pass. A test that fails on *every* platform at
once is the test's own fault, and a local green run proves nothing when
the local host has the dependency CI does not.

**Checked and found not to be wrong.** The `scribe` enum's `Status`
variant looked like a declared-but-never-dispatched branch — a command
advertised in `--help` that does not exist. It is not: the variant
belongs to `InboxAction`, it is dispatched, and `hs scribe inbox status`
works, asking systemd for the unit's state — the right signal. Checked
rather than assumed; it changed nothing.

**Research.**
`docs/loop/runs/2026-10-21-07/research.md` — the problem measured, the
root cause, the literature, the technique chosen.
`docs/loop/runs/2026-10-21-07/eval.md` — the before/after, the
end-to-end evidence, the timings, and the tests.

**Version.** `v0.0.1-rc.375` — a patch-level bump. It adds one
optional field to a health response and changes what a status command
reports. It touches no stored data, no collection schema and no search
behaviour.

**Release.** `v0.0.1-rc.375`, tagged from `3034253` on `main`:
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.375> — 33
assets, the same five build targets and both Docker images, all twelve
jobs green. The published `hs` binary was downloaded and checked: it
reports `hs 0.0.1-rc.375`, carries the `0.0.1-rc.375` version marker
and is x86-64; the published `hs-distill-server` — the binary that
carries the fix — carries the same marker.

**Commit.** `bf62884` — "fix(status): the server's own answer says
whether it is running", with `62cedb9` fixing the test CI caught.

**Not deployed.** The live services were not upgraded: this iteration's
rules say not to run `hs upgrade` on live services. The fleet is still
on rc.373 (`big`, `two`, `bmb`, `mac_air`, `big_mac` all report
`hs 0.0.1-rc.373`), so the live `hs distill status` still reads the
stale PID file until the fleet is upgraded. The A/B was run instead with
the new binary on an alternate port against the same Qdrant, with the
live service on `:7434` untouched.

**Note for whoever deploys it.** rc.374 and rc.375 are two independent
changes and the fleet is on neither. rc.374 fixed the reference-list
demoter (the first hit of a query is prose, not a bibliography, and
RECALL@10 rose from 96 % to 100 %); rc.375 fixed the status command's
liveness report. Upgrading to rc.375 gets both, because rc.375 contains
rc.374's change.


### Top 5 remaining items

1. **Cross-encoder reranking.** The GPU blocker has cleared — 18 GB of
   the 24.6 GB on `big` was free on 2026-10-21 — but the reranker is
   **not in home-still at all**: no code path, no client, no service, no
   endpoint. The model is on disk
   (`~/models/bge-reranker-v2-m3-GGUF/bge-reranker-v2-m3-Q8_0.gguf`) and
   nothing serves it. This is the next *large* piece of work: a GPU
   service, a serving path, a client, a latency budget, and a new
   failure mode. It is the right long-term answer for the *order* of
   the prose — the reference-list noise is now 4 % of the first hits.
2. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
3. **Source scopes (#4).** The MCP layer has no notion of which source
   a document came from (academic, personal, internet, synthesis,
   iMessages) and no per-source scope, so every tool returns every
   document. It is what stops a personal document leaking through a
   general search, and it is what would give a personal document a
   shorter link TTL.
4. **The `personal` store behind `Storage`.** The personal store sits
   on the local filesystem outside `Storage`, so the docs service
   cannot serve a personal document — which is also why issue #7's
   "personal documents get links" is not started. Moving it behind
   `Storage` (a `hs migrate personal-to-storage`) would let one read
   path serve every document the corpus holds.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.
## 2026-10-21-08 — the indexer that cannot index a document

**Change.** The distill indexer was **stuck**. On `big`, the event
watcher was in a 30-second backoff loop, redelivering the same three
documents over and over, and they never indexed. The server's log named
the cause exactly: `Failed to allocate memory for requested buffer of
size 2097152000`.

That number is not a mystery — it is the **attention scratch buffer**
of one forward pass, `rows × sequence² × heads × 4 bytes`:

```
20 rows × 1280² × 16 heads × 4 bytes = 2,097,152,000 bytes = 2.10 GB
```

precisely the `2097152000` in the log (the 1.15 GB and 1.47 GB
allocations are the same formula at 14 and 18 rows). The server asked
the shared card for a 2.10 GB block and could not have it.

Three separate defects each made the failure permanent:

1. **The VRAM gate measured the wrong thing.** `require_vram` runs
   *before the model is loaded* and checks that the **model** fits
   (5000 MB, "bge-m3 needs ~4.4 GB resident"). The **batch's** 2.10 GB
   scratch is allocated later, during the forward pass, at a moment the
   gate has already passed.
2. **The adaptive batch controller learned from successes only.**
   `observe()` is called on the success path, so a batch that fails to
   allocate taught it nothing — it stayed at the size that failed, and
   every redelivery failed identically.
3. **The watcher redelivered forever.** A 500 is classified
   *transient* (only an invalid key or a 400/413/422 is permanent), so
   the event was NAKed with a 30-second backoff and asked for again,
   indefinitely.

**Why this ranked first.** It beat the runner-up — **cross-encoder
reranking**, the backlog's first item, whose GPU blocker has now
cleared — on the user's own ranking of harm. A cross-encoder
**reorders** results; measured on the fixed query set it moved the
expected passage's rank on **4 queries better and 3 worse**, for a
**5× latency increase** (162 ms → 776 ms median) and a new GPU
service, a new model and a new failure mode. The indexer failure means
a document is **not in the library at all**: the user asks a question
about it and gets nothing. That is a tier above a reordering. The
cross-encoder's own prior art predicted the null — our sibling crate's
M37 measured **+0.78 points, 95 % CI [−3.14, +4.71]** — and this
iteration measured the same null on home-still's own corpus.

**The change, in three parts.**

* **The embedder retries at a smaller batch.** An ONNX allocation
  failure halves the batch and retries, down to a floor of one row. The
  descent is *per chunk*, so a batch that dropped to 3 descends again
  for its remainder. The retry is safe because the caller upserts by
  `(doc_id, chunk_index)`: a row embedded twice lands on the same key
  with the same vector.
* **The controller learns from failures.** `observe_failure()` steps
  the batch down and clears a "converged" verdict — the missing half
  of the controller.
* **The batch is sized to the card, before the attempt.** The free
  VRAM is divided by the scratch one row needs, and the batch is capped
  by the result, so a shared card produces a smaller batch rather than
  an arena failure.

**The evidence.** The cap is **live and visible in the server's log** —
the new server, run against the live corpus with the card deliberately
crowded by three extra `llama-server` processes, logged:

```
INFO the card cannot hold the configured batch; using a smaller one for
     this pass configured=20 cap=17
INFO … configured=20 cap=2
INFO … configured=20 cap=5
INFO … configured=20 cap=11
```

and the three documents the watcher was redelivering — fetched from S3
the way the watcher fetches them, and indexed with the same `/distill`
request — **all indexed**, with the card at 212 MB free:

| document | the live server (rc.373) | the new server (rc.376) |
|---|---|---|
| `10.1136_amiajnl-2011-000089` | OK in 24,823 ms | OK in 20,294 ms |
| `10.1016_j.brat.2014.06.007` | OK in 21,611 ms | OK in 10,770 ms |
| `10.51380_gujr-40-04-10` | OK in 11,172 ms | OK in 12,509 ms |

**The honest statement of what was measured.** The cap is observed
working in the live server's log. The retry is covered by unit tests
against the real `embed_in_batches`. The live failures were
*intermittent* — 24 in the watcher's journal over the day, on 8
distinct stems — and could not be forced on demand with a synthetic
tenant, because ONNX's arena grows to fit rather than failing. What is
not in doubt is the arithmetic: the allocation the arena names is
exactly the attention scratch of the configured batch, and the new code
computes that number and sizes the batch to it before the attempt.

**One finding that changed the design.** The free VRAM is *not* what
the arena needs: at 12.6 GB free, 126 rows would fit by the free-VRAM
arithmetic, and yet the arena could not get a **contiguous** 2.1 GB
block, because the card was fragmented by three other processes. So a
free-VRAM cap alone would **not** have prevented these failures. The
cap is the cheap, preventive half; the **retry** is the half that
covers the residual, and it is the half that must be there.

**The tests.** Five for the retry, in
`crates/hs-distill/src/embed/mod.rs`: the ONNX message is recognised
(and a failure that is *not* an allocation failure is not, so a
malformed input is not hidden behind a batch-size story); the batch
halves and completes; **the order is preserved and every row is
embedded exactly once** (the invariant that matters — a shrunk batch
must not reorder or drop a row); a non-allocation failure is not
retried; and at the floor the error is the real one. Three for the
controller, in `crates/hs-distill/src/adaptive_batch.rs`: a failure
steps the batch down; a failure at the smallest candidate stays there
and says so; and a failure clears a "converged" controller.

**Research.**
`docs/loop/runs/2026-10-21-08/research.md` — the problem measured,
the arithmetic, the three defects, the corpus's prior art (the
idempotence and "just crash" passages from *Designing Data-Intensive
Applications* and *A Philosophy of Software Design*), and the technique
chosen. `docs/loop/runs/2026-10-21-08/eval.md` — the before/after, the
end-to-end evidence, the timings, and the tests.

**Version.** `v0.0.1-rc.376` — a patch-level bump. It changes how the
embedder sizes a batch and what it does when the accelerator refuses
one. It touches no stored data, no collection schema, no search
behaviour and no wire format.

**Commit.** `23a1cf6` — "fix(embed): a batch the card cannot hold is
retried smaller".

**The query set does not move.** The change is in the embedder's batch
loop and the controller, not in retrieval. Re-run on the new server:
RECALL@10 **25/25 = 100 %** — the same as rc.374 measured, and the
live rc.373 server gives 24/25 (the difference is rc.374's demoter
fix, which is in the new server and not in the deployed one).

**Release.** `v0.0.1-rc.376`, tagged from `23a1cf6` on `main`:
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.376> — 33
assets, the same five build targets and both Docker images, all twelve
jobs green. The published `hs` binary was downloaded and checked: it
reports `hs 0.0.1-rc.376`, carries the `0.0.1-rc.376` version marker
and is x86-64; the published `hs-distill-server` — the binary that
carries the fix — carries the same marker.

**Not deployed.** The live services were not upgraded: this iteration's
rules say not to run `hs upgrade` on live services. The new binary was
run on an alternate port against the same Qdrant and the same CUDA
device, with the live service on `:7434` untouched.


### Top 5 remaining items

1. **The embedder's `idle_release_secs`.** The card cannot host the
   olmocr backend and a resident embedder at once: 14,446 MB free
   against a 13,500 MB headroom is a **946 MB surplus**, smaller than
   the `bench_elements` process another agent runs. Measured again on
   2026-10-22 during the rc.379 deploy: the card had **18,171 MB**
   free and both tiers were healthy — the surplus appears and vanishes
   with other tenants. `vram_floor_mb` already exists as a key; letting
   the embedder *yield* under contention would give the converter the
   room it needs. This is the other half of issue #10.
2. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client, no
   service, no endpoint. Measured on this corpus, 2026-10-21: it moved
   the expected passage's rank on **4 queries better and 3 worse** for
   a **5× latency increase** (162 → 776 ms median) — and the sibling
   crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model: a serving
   path, a latency budget, and a decision about where it runs.
2. **The `personal` store behind `Storage`.** The personal store sits
   on the local filesystem outside `Storage`, so the docs service
   cannot serve a personal document — which is also why issue #7's
   "personal documents get links" is not started. Moving it behind
   `Storage` (a `hs migrate personal-to-storage`) would let one read
   path serve every document the corpus holds.
3. **Source scopes (#4).** The MCP layer has no notion of which source
   a document came from (academic, personal, internet, synthesis,
   iMessages) and no per-source scope, so every tool returns every
   document. It is what stops a personal document leaking through a
   general search, and it is what would give a personal document a
   shorter link TTL.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.
## 2026-10-21-09 — a personal document the agent cannot show you

**Change.** The wishlist says: *"whenever an agent calls search and
pulls text, it is also given links to documents that the user of the
agent can view"*, and *"a really cool feature would be able to highlight
the text in the document when all this needs to be served remotely"*.

**That was already true for the academic corpus, and false for the
personal store.** Measured by driving `hs-mcp` over stdio the way
Claude Desktop does, 2026-10-21:

| the tool | hits that carry links (before) |
|---|---|
| `distill_search` | **5/5** ✓ |
| `abstract_search` | **5/5** ✓ |
| `personal_search` | **0/5** ✗ |

A `personal_search` hit came back with only `stem`, `title`,
`category`, `snippet`, `score` — the agent had the text and **nothing
the user could open**.

**Three causes, all measured.**

1. **The personal store is outside `Storage`.** It writes through raw
   `std::fs` (`personal/src/services/catalog.rs:91`, `:203`;
   `personal/src/services/ingest.rs:48`). The docs service reads
   *everything* through one `Storage`
   (`crates/hs/src/docs_service.rs:55`).
2. **The personal store is not in the bucket.** `aws s3 ls
   s3://home-still/` on `big` lists `.heartbeats/`, `10/`, `catalog/`,
   `corrupted/`, `markdown/`, `papers/`, `research/` — and **no
   `personal/`**.
3. **It is on one host.** `two`, `bmb` and `mac_air` have **0**
   personal files; `big` has 52. A medical record lived in one
   directory on one machine.

**The change, in four parts.**

* **The link carries the collection.** The signed payload gains a
  `collection` field, so a link names *which* store it is for. It is
  signed like the page: a link for a personal document cannot be
  edited into a link for an academic one.
* **The docs service resolves the collection** to the prefixes that
  serve it. A collection it does not serve is a **404** — the link is
  genuine, but it names a store this host does not have.
* **`hs migrate personal-to-storage`** moves the personal objects into
  the corpus's own store, under the `personal/` prefix. **Dry-run by
  default**, and it **deletes nothing**: the local files stay where
  they are.
* **`personal_search` mints links**, the way `distill_search` does.

**The key observation that made it one change and not three.** The
personal store's on-disk layout is *already* the corpus's layout, one
prefix deeper: the sources are `personal/<shard>/<stem>.pdf` and the
converted text `personal/markdown/<shard>/<stem>.md`. The docs service
already serves any stem through `Storage`; it just did not look under
a third prefix. So the backlog's items 2 and 3 — "the personal store
behind `Storage`" and "source scopes" — are **one** change, and the
wishlist's gap is the same change seen from the user's side.

**The evidence.**

| | before | after |
|---|---|---|
| `personal_search` hits carrying a link | **0/5** | **5/5** |
| the personal link serves the document | — | **200**, 8,721 bytes of the real record |
| a personal document's source | — | **200**, 1,028,277 bytes, `%PDF-1.4` |
| the same stem in the wrong collection | — | **404** (not the other store's document) |
| a link whose collection was edited in the query | — | **403 link refused** (the collection is signed) |
| the personal store in the bucket | **no `personal/` prefix** | **321 files moved** |
| the personal store on the other hosts | **0 files** | the same bucket |

**The migration, run against the live store** — the dry run first, as
the rules require:

```
$ hs migrate personal-to-storage --dry-run
Plan: 321 personal file(s) found; 321 planned this run (dry-run)
Sample: personal/a-/a-beginners-guide-to-dual-quaternions-0287e220.pdf
Dry-run: 321 file(s) would be written under personal/

$ hs migrate personal-to-storage
Moved: 321
Skipped: 0
The personal store is now under personal/ in the corpus's storage. The local files were left in place.
```

**Why this ranked first.** It beat the runner-up — **cross-encoder
reranking**, the backlog's first item, whose GPU blocker has cleared —
on the user's own ranking of harm. A cross-encoder *reorders* the
results of a search that works: measured on this corpus, 2026-10-21, it
moved the expected passage's rank on **4 queries better and 3 worse**
for a **5× latency increase** (162 → 776 ms median), and the sibling
crate's own M37 measured the same null (+0.78 points, CI [−3.14,
+4.71]). This fixes the other half: a search that works but whose
results the user **cannot open**. The user's own words put it first.

**The research, in the corpus.** The multi-tenancy table in *Reusing
empirical knowledge during cloud computing adoption* states the
requirement as a cloud adoption obstacle, T10 "Isolate tenant":
*"define tenant-based identification and hierarchical access control for
tenants and (ii) separate tenant data using authorization and
authentication mechanisms."* The two halves are exactly what was
missing: the personal store had **neither** a tenant identification in
its links nor its data in the shared storage. And *Model Context
Protocol (MCP): Landscape, Security Threats, and Future Research
Directions* names the risk class the change walks into — MCP's
openness "exposes new attack surfaces, such as … tool poisoning", and
*"an AI agent can only act upon malicious instructions, when it has
tools to act"*. A link the agent hands the user is a *capability*: it
must grant access to one document, in one store, for a bounded time.
Putting the collection in the signature is what makes it one.

**The tests.** Three for the link (`hs-common/src/docs.rs`): a
tampered collection does not verify; a link without a collection is
refused; a collection that is not a stem is refused. Three for the
service (`crates/hs/src/docs_service.rs`): a personal document is
served by its own link; a link cannot be edited from one collection to
the other; a collection the service does not serve is a 404. One for
the migration (`crates/hs/src/migrate_personal.rs`): the files keep
their names under the new prefix.

**Research.**
`docs/loop/runs/2026-10-21-09/research.md` — the problem measured,
the three causes, the corpus's prior art, and the technique chosen.
`docs/loop/runs/2026-10-21-09/eval.md` — the before/after, the
end-to-end evidence, the timings, and the tests.

**Version.** `v0.0.1-rc.377` — a patch-level bump. It adds one field
to a signed link and one collection to the docs service's
configuration. It touches no stored data's *content* (the migration
copies, it does not rewrite) and no search behaviour.

**Commit.** `5b9532b` — "feat(docs): a personal document the agent can
show the user", with `e7fca05` fixing two test attributes clippy
caught.

**Release.** `v0.0.1-rc.377`, tagged from `6d0a191` on `main`:
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.377> — 33
assets, the same five build targets and both Docker images, all twelve
jobs green. The published `hs` binary was downloaded and checked: it
reports `hs 0.0.1-rc.377`, carries the `0.0.1-rc.377` version marker
and is x86-64; the published `hs-mcp` — the binary that mints the links
— carries the same marker.

**Note for whoever deploys it.** The link's signature shape changed:
the collection is now in it. A link minted by an **older** service is
refused by a newer one, and the other way round. The links are
short-lived (an hour by default), so the window is small — but the
docs service and the MCP server must be upgraded **together**.

**The migration has been run; the fleet has not been upgraded.** The
321 files are in the bucket — `aws s3 ls s3://home-still/` now lists
the `personal/` prefix, and the store is shared by every host and
covered by the backup. The *binaries* are still rc.373 on all five
hosts, so the live `personal_search` still returns no links until the
MCP server is upgraded. The rules say not to run `hs upgrade` on live
services, so that is the operator's step.


### Top 5 remaining items

1. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client,
   no service, no endpoint. Measured on this corpus, 2026-10-21: it
   moved the expected passage's rank on **4 queries better and 3 worse**
   for a **5× latency increase** (162 → 776 ms median) — and the
   sibling crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model: a serving
   path, a latency budget, and a decision about where it runs.
2. **The personal store's *reads* behind `Storage`.** The objects now
   live in the corpus's store (rc.377) and the docs service serves
   them, but the personal crate still reads and writes its own
   catalog through `std::fs` — so a personal document can be in the
   bucket and still not be found by `hs personal read` on a host whose
   disk does not have it. The migration moved the *data*; the *code
   path* is the rest of the work.
3. **Source scopes (#4), the rest of it.** The links now name a
   collection, so a link is a capability for one store. What is still
   missing is the other half: a *search* that says which stores it
   looks in, so a general search does not return a game-engine book
   for "risk factors for falls in elderly people" — measured
   2026-10-21, the personal collection answers that query with *The
   Base Class for Game Objects in SSN67*.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.
## 2026-10-21-10 — the document that is in the bucket and cannot be read

**Change.** The previous iteration (rc.377) moved the personal store's
**objects** into the corpus's storage, under the `personal/` prefix, and
the docs service now serves them with signed links. The *data* moved.
The **code path** did not: the personal crate still read and wrote its
catalog through raw `std::fs`.

Measured on the fleet, with the objects already in the bucket:

```
$ hs personal read casey-brittain-medical-history-overview-65004350
  big       patient: Casey Brittain …        (the local files are here)
  two       error: no markdown found for stem 'casey-brittain-…'
  bmb       error: no markdown found for stem 'casey-brittain-…'
  mac_air   patient: Casey Brittain …        (it has its own copy)

$ hs personal list
  two       0 entries shown
```

**So a personal document was in the bucket, was served by the docs
service, was findable by the *search* (which goes through the distill
server) — and still could not be *read* or *listed* on two of the five
hosts.** The store was half in the corpus and half on one machine, and
the half that was on one machine was the half that answered "what do
you have?".

**Why this ranked first.** It beat the runner-up — **cross-encoder
reranking**, the backlog's first item, whose GPU blocker has cleared —
on the user's own ranking of harm. A cross-encoder *reorders* the
results of a search that works: measured on this corpus, 2026-10-21, it
moved the expected passage's rank on **4 queries better and 3 worse**
for a **5× latency increase** (162 → 776 ms median) and a new GPU
service, a new model and a new failure mode; the sibling crate's own
M37 measured the same null (+0.78 points, CI [−3.14, +4.71]). This is
a **document that is not there**: the user asks an agent about their
medical history, the agent finds the passage, and then the agent cannot
open the document it found — on two of the five hosts. That is a tier
above a reordering.

It is also the **other half of rc.377's own change**. The migration
moved the data and this moves the code path that reads it; the two are
one piece of work, and the first was done without the second, which is
why the store was in the bucket and out of reach.

**The change.** The personal store's catalog goes through `Storage`, the
way the paper crate's has for a long time:

* `Config` gains a `storage:` section, read from the **top-level**
  `storage:` section of the config file — the same one the corpus uses
  — and a `build_storage()` that returns `Arc<dyn Storage>`.
* The catalog's reads and writes go through `Storage`, at the keys the
  migration already produced: `personal/<shard>/<stem>.catalog.yaml` and
  `personal/markdown/<shard>/<stem>.md`.
* The **ingest** writes its objects to the store too, so a newly
  ingested document lands where the readers look.
* The **inbox stays local, on purpose**: it is a *drop folder*, the
  one place an external agent may introduce bytes, and the config
  documents it as such ("absolute values are taken as-is so a user can
  point the inbox at, e.g., an NFS drop folder shared with a phone
  scanner"). That is a local path by design, and it stays one.

**The proof, by removing the local files.** On `big`, the local
personal store was moved aside (reversibly) and both binaries were
asked for the same document:

```
$ mv /mnt/home-still/personal /mnt/home-still/personal-local-backup

$ hs personal read casey-brittain-medical-history-overview-65004350   # rc.373
error: Error: no markdown found for stem 'casey-brittain-medical-history-overview-65004350'
$ hs personal list                                                   # rc.373
0 entries shown

$ hs personal read casey-brittain-medical-history-overview-65004350   # rc.378
---
patient: Casey Brittain
source: Epic MyChart EHI export (received 2025-04-17)
domain: medical/overview
$ hs personal list                                                   # rc.378
Effects Techniques in Uncharted 3       other   effects-techniques-in-uncharted-3-3302b34d
LBP Engine Requirements and Lighting …  other   lbp-engine-requirements-and-lighting-9a90548d
…
```

**With the local files gone, the old binary finds nothing and the new
one finds everything** — from the bucket, over the network, with the
same command. The local store was then moved back and the old binary
works again, so nothing was lost.

**A configuration that would have silently split the store is now
refused.** The shared `ConfigFile::default_storage()` refuses an
implicit storage root that differs from `home.project_dir`, and the
error names the fix.

**The tests.** The personal crate's 51 tests pass, and they were
updated to write through the store — the helpers put their objects at
the storage keys the readers use, so a test that passes is a test of
the real layout. The configuration test now asserts the store's paths
*and* the storage's root, and a new one asserts that a project
directory without a `storage:` section is refused. The four behaviours
that had to survive — the same content under a new title is a
duplicate, a failing index call does not cost the document its
vectors, an abandoned ingest cleans up after itself, `--force` replaces
the old stem — are the ones that caught this change being wrong while
it was being written.

**Research.**
`docs/loop/runs/2026-10-21-10/research.md` — the problem measured,
the three causes, the corpus's prior art (the interface-versus-
implementation and pass-through-methods passages from *A Philosophy of
Software Design*), and the technique chosen.
`docs/loop/runs/2026-10-21-10/eval.md` — the before/after, the proof,
the timings, and the tests.

**Version.** `v0.0.1-rc.378` — a patch-level bump. It changes where
the personal store's catalog is read from and written to. It touches no
stored data's content, no collection schema and no search behaviour.

**The query set does not move**, as it must not: the change is in the
personal store's catalog, not in retrieval. Re-run on the live server:
RECALL@10 **24/25 = 96 %** — the same as before this change.

**Commit.** `db196c4` — "fix(personal): the store's catalog goes through
the corpus's storage".

**Release.** `v0.0.1-rc.378`, tagged from `39864a7` on `main`:
<https://github.com/home-still/home/releases/tag/v0.0.1-rc.378> — 33
assets, the same five build targets and both Docker images, all twelve
jobs green. The published `hs` binary was downloaded and checked: it
reports `hs 0.0.1-rc.378`, carries the `0.0.1-rc.378` version marker
and is x86-64.

**Note for whoever deploys it.** The personal store's config must have
a `storage:` section, or `home.project_dir` must be the default root.
`two` and `bmb` already have the S3 section; `big` has the local
backend rooted at the project directory. The error names the fix when
it is missing.


### Top 5 remaining items

1. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client,
   no service, no endpoint. Measured on this corpus, 2026-10-21: it
   moved the expected passage's rank on **4 queries better and 3 worse**
   for a **5× latency increase** (162 → 776 ms median) — and the
   sibling crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model: a serving
   path, a latency budget, and a decision about where it runs.
2. **Source scopes (#4).** The links now name a collection, so a link
   is a capability for one store, and the personal store is now read
   through the corpus's storage. What is still missing is the other
   half: a *search* that says which stores it looks in, so a general
   search does not return a game-engine book for "risk factors for
   falls in elderly people" — measured 2026-10-21, the personal
   collection answers that query with *The Base Class for Game Objects
   in SSN67*.
3. **The personal store's *ingest* through `Storage`.** The catalog
   now reads and writes the store (rc.378); the **ingest** still reads
   the source from the local inbox and converts it there. The inbox is
   a local drop folder by design, but the *conversion* could be a
   server-side job, so a host that cannot reach the scribe server could
   still hand a document over.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.
## 2026-10-22-02 — the status that said "backend unavailable" and nothing else

**Change.** `hs status` reported a scribe tier as **"backend
unavailable"** and nothing else. The tier was refusing work, the corpus
was not growing on it, and the dashboard never said why. The cause was in
the scribe's own journal, on another host.

**The numbers were already in the payload the client parses.**
`ScribeClient::health` reads `backend_vram_free_mb`,
`backend_model_resident` and `backend_checked_at` — and the MCP's status
builder **dropped all three**, replacing them with the string
`"backend unavailable"`. The information was fetched, parsed, and
discarded.

Measured on the live `hs status`, 2026-10-22:

```
Services:
  ○ scribe   http://192.168.1.110:7435            backend unavailable   0.0.1-rc.373
  ● scribe   http://bmb.local:7433                5 converting   0.0.1-rc.373
```

and in the tier's journal, on another host:

```
WARN vlm backend unavailable — refusing dispatch free_vram_mb=Some(13251)
     reachable=true headroom_mb=13500 holders=pid=2227527 7534MB
     hs-distill-server; pid=1737565 2596MB hs-scribe-server;
     pid=2615361 591MB bench_elements
```

**How much it cost.** Over the last 12 hours on `big`: **54 refusals**,
and **57 of the 107 gate readings (53 %) were under the headroom** — the
tier refused more often than it passed. The free VRAM at the refusals
ranged 6,314–13,345 MB against a headroom of 13,500 MB. The tier still
did 146 conversions, so the pipeline was not stopped — it was quietly
running at a fraction of its capacity, for a reason nobody could see.

**The arithmetic, for the record.** The card is 24,576 MB. The distill
server's bge-m3 embedder holds 7,534 MB resident, the scribe server
2,596 MB, and the olmocr backend wants ~15.4 GB at
`--gpu-memory-utilization 0.65` (the `llama-swap.yaml` comment says
0.60 — the two disagree, and that is a host-side finding, not a
repository one). The free VRAM with only our own services resident is
14,446 MB and the gate wants 13,500: **a surplus of 946 MB**, which is
smaller than the `bench_elements` process (591 MB) another agent runs.
**Every one of the 54 refusals had a third-party tenant holding the
room.**

**Why this ranked first.** It beat the runner-up — **cross-encoder
reranking**, the backlog's first item, whose GPU blocker has cleared —
on the user's own ranking of harm, and on an axis the last three
iterations did not touch:

* A cross-encoder *reorders* the results of a search that works.
  Measured on this corpus, 2026-10-21: it moved the expected passage's
  rank on **4 queries better and 3 worse**, for a **5× latency
  increase** (162 → 776 ms median), and the sibling crate's own M37
  measured the same null (+0.78 points, CI [−3.14, +4.71]).
* This is a **blind spot in the only dashboard an operator has**. The
  corpus is not growing on one of two scribe tiers, and the tool that
  says so says "backend unavailable" — which is indistinguishable from
  a crashed server, a missing model, a wrong URL or a broken
  converter. The operator cannot act on it: the next step is to read a
  journal on another host.

**It is also issue #10's fourth bullet**, which another agent filed on
2026-10-21 with the same diagnosis: *"Report the tier's VRAM gate in
`system_status`. `free_vram_mb` and `model_resident` are already logged
internally … surfacing them in the status payload would make this
diagnosable without reading another host's journal."* Of that issue's
four bullets this is the one that is **in this repository**: the others
are about the host's `llama-swap.yaml` and `run-olmocr.sh` (not in the
repo), the embedder's `idle_release_secs` (a config change on a shared
box), and the cold-start deadline.

**The change — report the cause, not the symptom.**

* The scribe server's `/health` now carries the **headroom it wants**
  and the **holders of the card**. It already carried the free VRAM;
  without the headroom, "12.5 GiB free" says nothing, and without the
  holders, "who is holding the card" is unanswered.
* The status line becomes a diagnosis: *"backend unavailable: 12.9 GiB
  free, needs 13.2 GiB; held by pid=2227527 7534MB hs-distill-server;
  …"*.

**The gate's behaviour is unchanged.** Every one of the 54 refusals had
a genuine shortfall, and refusing work the card cannot do is right. This
changes what is *reported*, not what is *fetched* — the numbers were
already parsed and thrown away.

**The evidence.** The before and the after, from the same MCP call:

```
  the old MCP: backend unavailable
  the new MCP: backend unavailable: 13.0 GiB free
```

and the whole diagnosis, end to end, against the payload the new scribe
server sends:

```
  http://127.0.0.1:46225
    -> backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
       pid=2227527 7534MB hs-distill-server; pid=1737565 2596MB
       hs-scribe-server; pid=2615361 591MB bench_elements
```

**One path.** `hs status` (the CLI) and the MCP's `system_status` go
through the *same* code — the CLI calls the MCP's `system_status` tool
(`crates/hs/src/status_cmd.rs:242`) — so both carry the diagnosis.

**A model that is already resident needs no room to start**, so no
headroom is reported against it: the gate's free-VRAM half does not
apply then, and reporting a shortfall that is not one would be a lie of
the same kind this change fixes.

**The tests.** Four, in `crates/hs-mcp/src/main.rs::backend_reason_tests`:
the free VRAM *and* the headroom, both in GiB; the holders and their
sizes; a resident model reports no requirement, because it needs none;
and a health body with no numbers still says something true — the
diagnosis must never be a guess.

**Research.**
`docs/loop/runs/2026-10-22-02/research.md` — the problem measured,
the arithmetic, the corpus's prior art (the fault/failure and
observability passages from *A Philosophy of Software Design*, and the
"failure observability and repairability" lens from *Agentic Artifact
Creation*), and the technique chosen.
`docs/loop/runs/2026-10-22-02/eval.md` — the before/after, the
end-to-end evidence, and the tests.

**Version.** `v0.0.1-rc.379` — a patch-level bump. It adds two
optional fields to a health response and changes one status line. It
touches no stored data, no collection schema, no search behaviour and
no wire format's meaning.

**The query set does not move**, as it must not: the change is in what
the status reports, not in retrieval. Re-run on the live server:
RECALL@10 **24/25 = 96 %** — the same as before this change.

**Commit.** `ba36f36` — "feat(status): the scribe's VRAM gate says why,
not just that".

**Release.** `v0.0.1-rc.379`, tagged from `ba36f36` on `main`. The
CI gate is green on ubuntu-24.04, macos-14 and windows-2022, and the
release published at 12:33:11Z.

**The artifacts, checked.** The `hs-version-marker` is the thing rc.359
shipped without on darwin, so it is the first thing to look at:

| the artifact | the marker | the arch |
|---|---|---|
| `hs` (aarch64-apple-darwin) | `hs-version-marker:0.0.1-rc.379:end` | arm64 Mach-O |
| `hs-mcp` (aarch64-apple-darwin) | `hs-version-marker:0.0.1-rc.379:end` | arm64 Mach-O |
| `hs-scribe-server` (aarch64-apple-darwin) | `hs-version-marker:0.0.1-rc.379:end` | arm64 Mach-O |
| `hs` (x86_64-unknown-linux-gnu) | `hs-version-marker:0.0.1-rc.379:end` | x86-64 ELF |
| `hs-mcp` (x86_64-unknown-linux-gnu) | `hs-version-marker:0.0.1-rc.379:end` | x86-64 ELF |
| `hs-distill-server` (x86_64-unknown-linux-gnu) | `hs-version-marker:0.0.1-rc.379:end` | x86-64 ELF |

`hs --version` prints `hs 0.0.1-rc.379` and `hs-mcp --version` prints
`hs-mcp 0.0.1-rc.379` from the published tarball. The checksums verify
against `checksums-sha256.txt` (16 entries). **rc.359's failure mode did
not recur.**

**Not deployed.** The live services were not upgraded: this iteration's
rules say not to run `hs upgrade` on live services. The new scribe
server was run on `:7439` and the new MCP by hand, against the same
corpus and the same GPU.

**Note for whoever deploys it.** The scribe server and the MCP must be
upgraded **together** for the diagnosis to appear: the MCP asks the
server for the headroom, and an older server does not send it. The
MCP's own fallback is the old wording, so a mixed pair degrades to
today's behaviour rather than breaking.


### Top 5 remaining items

1. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client,
   no service, no endpoint. Measured on this corpus, 2026-10-21: it
   moved the expected passage's rank on **4 queries better and 3 worse**
   for a **5× latency increase** (162 → 776 ms median) — and the
   sibling crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model: a serving
   path, a latency budget, and a decision about where it runs.
2. **The embedder's `idle_release_secs`.** The card cannot host the
   olmocr backend and a resident embedder at once: 14,446 MB free
   against a 13,500 MB headroom is a **946 MB surplus**, smaller than
   the `bench_elements` process another agent runs. `vram_floor_mb`
   already exists as a key; letting the embedder *yield* under
   contention — release the model when the card is short — would let
   the converter have the room it needs. This is the other half of
   issue #10, and it is a config-and-code change on a shared box.
3. **Source scopes (#4).** The links now name a collection, so a link
   is a capability for one store, and the personal store is read
   through the corpus's storage. What is still missing is the other
   half: a *search* that says which stores it looks in, so a general
   search does not return a game-engine book for "risk factors for
   falls in elderly people" — measured 2026-10-21, the personal
   collection answers that query with *The Base Class for Game Objects
   in SSN67*.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.

## 2026-10-22-03 — the diagnosis the dashboard threw away

**Change.** rc.379 made the scribe's VRAM gate say *why* it was refusing
work. rc.380 fixes the dashboard that was throwing the answer away: the
Services table's Status cell was a **fixed 24 characters**, so the
167-character diagnosis — the free VRAM, the headroom needed and every
holder of the card — was cut to `backend unavailable: 12.`.

**How it was found.** The release skill's Gate 7 (a regression-class
sweep: grep for the pattern the rc fixed, confirm zero remaining sites)
found the second site. The text rendering (`hs status > file`) was
correct; the **TUI** was not, and the TUI is what an operator actually
looks at.

**Measured.** The diagnosis is 167 characters; the cell was 24. **143
characters were lost** — every number and every holder.

```
  before:  backend unavailable: 12.
  after:   backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
           pid=2227527 7534MB hs-distill-server; pid=1737565 2596MB
           hs-scribe-server; pid=2615361 591MB bench_elements
```

**Three ratatui facts the fix turned on**, each found by reading the
widget's source rather than guessing:

1. **A table cell truncates; it does not wrap.** `Cell::from(Text)`
   with a long string is cut at the cell's width. The text is wrapped
   into `Vec<Line>` here, at the column's width, on word boundaries.
2. **A row is ONE line tall unless told otherwise.** `Row::height`
   defaults to `1` (`ratatui-0.29.0/src/widgets/table/row.rs:108`), so
   a wrapped cell is cut after its first line. The row's height is set
   to the wrapped line count.
3. **`Constraint::Min(24)` is a floor, not a claim.** The layout's
   cassowary solver hands the surplus to the `Fill` constraints, so the
   Status column stayed at 24. The width is therefore **computed from
   the content** (`services_status_width`), not negotiated.

**A second defect the fix exposed.** Giving the Status column the room
it needs took the room from the URL: the rendered row showed
`http://scribe.exa`. The URL column is now sized from its content too
(`services_url_width`) and is **never truncated** — the operator reads
the URL to know which tier a row is about.

**The layout.** The Status column's width is the longest status line,
with a 24-character floor (so a short label looks exactly as it always
did) and a ceiling that leaves the URL whole. A diagnosis longer than
the terminal wraps onto more lines and the row gets taller — the honest
rendering, since the Services block's height is computed from the
wrapped line count.

**The tests.** Two, in `crates/hs/src/status_cmd.rs`:

* `the_dashboard_shows_the_whole_vram_diagnosis_not_a_truncated_label` —
  every **word** of the diagnosis survives, and the URL beside it is
  whole. (The assertion is on words, not on the whole phrase: wrapping
  legitimately breaks a phrase at a word boundary, and a test that
  demanded a contiguous substring would be testing the wrap width
  rather than the content.)
* `a_long_diagnosis_does_not_truncate_the_url` — the URL survives a long
  diagnosis.

**Both failed before the fix and pass after it** — the first one failed
twice, which is how the URL defect and the row-height defect were found.

**Version.** `v0.0.1-rc.380` — a patch-level bump. It changes how the
TUI lays out one table. It touches no stored data, no collection schema,
no search behaviour and no wire format.

**Deployed.** All eight reachable hosts are on rc.380 (`five` is
offline). See the deployment record below.

---

## The rc.380 deployment (2026-10-22, Gates 5-6)

**Gate 5 — `hs upgrade --pre --yes`, in series.** The same command as
rc.379, on the same eight hosts, all now on **rc.380**.

| the host | the result |
|---|---|
| `big` | rc.379 → **rc.380**, 9 services restarted |
| `bmb`, `mac_air`, `big_mac` | rc.379 → **rc.380** |
| `two`, `three`, `one` | rc.379 → **rc.380** |
| `four` | rc.379 → **rc.380** (see the transient below) |
| `five` | **offline** (`No route to host`) |

**Two real findings in the rollout**, both on `big` and `four`:

1. **The upgrade's Qdrant health wait is too short for a recovery.**
   `big`'s upgrade ended with `Error: Timed out waiting for Qdrant at
   http://localhost:6333/healthz (90s)`. The container was **up and
   healthy** — it was **recovering its collections** (`Recovering shard
   ./storage/collections/academic_papers/0`) and needed longer than the
   90-second wait. Qdrant answered 200 about seven minutes later, and
   the host was fully healthy. **The upgrade reported a failure on a
   healthy system**: the wait is `HEALTH_WAIT_SECS = 90`
   (`crates/hs/src/upgrade_cmd.rs:14`), and a corpus of 15,933 documents
   in three collections cannot be recovered in 90 s. The wait should be
   longer, or the message should say "still recovering" rather than
   "timed out". **Logged as a P0** — an upgrade that reports failure
   for a healthy host trains the operator to ignore it.
2. **`four` failed the release lookup twice, then succeeded.**
   `Error: No releases found` — with the GitHub API reachable from that
   host (a direct `curl` of the same URL returned rc.380, 10 releases,
   715,713 bytes, and the same `hs/0.0.1-rc.379` user agent made no
   difference). The third attempt worked. So the API path from `four`
   is **flaky**, and the installer's "No releases found" message does
   not say that: it says the *server* returned nothing, when the truth
   is that the request failed. **Logged as a P1.**

**Gate 6 — the changed surface, exercised on the live fleet.**

The card was contended again during the rollout, so the tier was
refusing work — which made the live fleet exercise the diagnosis for
real, with no test rig:

```
$ hs status          (the text rendering, on `big`, live)
  ○ scribe   http://192.168.1.110:7435   backend unavailable: 12.9 GiB free,
            needs 13.2 GiB; held by pid=749175 7734MB hs-distill-server;
            pid=715494 2464MB hs-scribe-server; pid=760998 585MB
            bench_elements   0.0.1-rc.380
  ● scribe   http://bmb.local:7433       idle   0.0.1-rc.380
  ● distill  http://192.168.1.110:7434   idle (Cuda)  0.0.1-rc.380
  ● qdrant   http://localhost:6334 → academic_papers  1.20.0
```

**And the TUI — the surface this rc fixed — on the same live data:**

```
  Scribe   ○  backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
           pid=749175 7734MB
           hs-distill-server; pid=715494 2464MB
           ...                       http://192.168.1.110:7435   0.0.1-rc.380
```

The diagnosis wraps across lines, the **URL is whole**
(`http://192.168.1.110:7435`, not `http://192.168.1`), and the version
is beside it. **Before rc.380 that row read `backend unavailable: 12.`
** — the free VRAM, the headroom and every holder were gone.

**One P0, already logged: P0-23.** `hs status` on `bmb`, `mac_air`,
`big_mac` and `two` fails with `gateway rejected the refresh token
(401)` — the **expired cloud enrollment**, not these rcs. It is in
`BACKLOG.md` as P0-23 and it needs an operator-issued invite code to
re-enroll, which an agent cannot do. The local services on those hosts
were verified directly instead (`bmb`'s scribe server: rc.380, `ok`;
`two`'s gateway: `/health` → 200).

---

## The rc.379 deployment (2026-10-22, the release skill's Gates 5-6)

**Gate 5 — `hs upgrade --pre --yes`, in series.** The flag is `--pre`
(the skill's Gate 5 says `--prerelease`; the CLI's own flag is `--pre`,
`crates/hs/src/cli.rs:75`).

| the host | the role | the result |
|---|---|---|
| `big` | the RTX 3090 host, both scribe tiers' origin | rc.373 → **rc.379**, 9 services restarted |
| `bmb` | the second scribe tier | rc.373 → **rc.379** |
| `mac_air` | a client | rc.373 → **rc.379** |
| `big_mac` | admin | rc.373 → **rc.379** |
| `two` | the gateway | rc.373 → **rc.379**, `hs-gateway.service` restarted |
| `three` | the NATS bus | rc.364 → **rc.379** |
| `one`, `four` | idle Pis | rc.364 → **rc.379** |
| `five` | — | **offline** (`No route to host`) |

`hs upgrade` covered `hs-mcp` and `hs-scribe-server` — the two binaries
this rc changed — on every host, and restarted every unit whose binary
was replaced. **No binary was hand-copied.** The docker path was
correctly skipped: **no Pi runs a home-still container** (`one` runs an
unrelated project, `three` and `four` run nothing, `four` runs Garage),
so there was no `ghcr.io/home-still/hs-scribe-server` to pull.

**Gate 6 — the changed surface, exercised.**

The live `hs status` on `big` after the upgrade:

```
Services:
  ● scribe   http://192.168.1.110:7435            idle   0.0.1-rc.379
  ● scribe   http://bmb.local:7433                idle   0.0.1-rc.379
  ● distill  http://192.168.1.110:7434            idle (Cuda)  0.0.1-rc.379
  ● qdrant   http://localhost:6334 → academic_papers  1.20.0
```

Both tiers green, distill on CUDA. And the live scribe server's own
`/health` now carries the new field — the change is **live**:

```json
{"status":"ok", …, "backend_vram_free_mb":18171,
 "backend_vram_headroom_mb":13500, "backend_model_resident":false, …}
```

**The refusal path, with the deployed binaries.** The card was free, so
the live fleet did not exercise the diagnosis. The **deployed** scribe
server was therefore started with a headroom it cannot meet (1 TB), so
its real gate refuses, and the **deployed** MCP read it back:

```
  its /health: status: backend_unavailable
               backend_vram_free_mb: 18171
               backend_vram_headroom_mb: 1048576
               backend_vram_holders: pid=167764 2622MB hs-distill-server;
                                     pid=166505 2594MB hs-scribe-server;
                                     pid=174521 585MB bench_elements

  the MCP:     -> backend unavailable: 17.7 GiB free, needs 1024.0 GiB;
                 held by pid=167764 2622MB hs-distill-server;
                 pid=166505 2594MB hs-scribe-server;
                 pid=174521 585MB bench_elements
```

**One P0, already logged: P0-23.** `hs status` on `bmb`, `mac_air`,
`big_mac` and `two` fails with `gateway rejected the refresh token
(401)` — the **expired cloud enrollment**, not this rc. It is in
`BACKLOG.md` as P0-23 and it needs an operator-issued invite code to
re-enroll, which an agent cannot do. The local services on those hosts
were verified directly instead (`bmb`'s scribe server: rc.379, `ok`;
`two`'s gateway: `/health` → 200).

## 2026-10-22-03 — the diagnosis the dashboard threw away

**Change.** rc.379 made the scribe's VRAM gate say *why* it was refusing
work. rc.380 fixes the dashboard that was throwing the answer away: the
Services table's Status cell was a **fixed 24 characters**, so the
167-character diagnosis — the free VRAM, the headroom needed and every
holder of the card — was cut to `backend unavailable: 12.`.

**How it was found.** The release skill's Gate 7 (a regression-class
sweep: grep for the pattern the rc fixed, confirm zero remaining sites)
found the second site. The text rendering (`hs status > file`) was
correct; the **TUI** was not, and the TUI is what an operator actually
looks at.

**Measured.** The diagnosis is 167 characters; the cell was 24. **143
characters were lost** — every number and every holder.

```
  before:  backend unavailable: 12.
  after:   backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
           pid=2227527 7534MB hs-distill-server; pid=1737565 2596MB
           hs-scribe-server; pid=2615361 591MB bench_elements
```

**Three ratatui facts the fix turned on**, each found by reading the
widget's source rather than guessing:

1. **A table cell truncates; it does not wrap.** `Cell::from(Text)`
   with a long string is cut at the cell's width. The text is wrapped
   into `Vec<Line>` here, at the column's width, on word boundaries.
2. **A row is ONE line tall unless told otherwise.** `Row::height`
   defaults to `1` (`ratatui-0.29.0/src/widgets/table/row.rs:108`), so
   a wrapped cell is cut after its first line. The row's height is set
   to the wrapped line count.
3. **`Constraint::Min(24)` is a floor, not a claim.** The layout's
   cassowary solver hands the surplus to the `Fill` constraints, so the
   Status column stayed at 24. The width is therefore **computed from
   the content** (`services_status_width`), not negotiated.

**A second defect the fix exposed.** Giving the Status column the room
it needs took the room from the URL: the rendered row showed
`http://scribe.exa`. The URL column is now sized from its content too
(`services_url_width`) and is **never truncated** — the operator reads
the URL to know which tier a row is about.

**The layout.** The Status column's width is the longest status line,
with a 24-character floor (so a short label looks exactly as it always
did) and a ceiling that leaves the URL whole. A diagnosis longer than
the terminal wraps onto more lines and the row gets taller — the honest
rendering, since the Services block's height is computed from the
wrapped line count.

**The tests.** Two, in `crates/hs/src/status_cmd.rs`:

* `the_dashboard_shows_the_whole_vram_diagnosis_not_a_truncated_label` —
  every **word** of the diagnosis survives, and the URL beside it is
  whole. (The assertion is on words, not on the whole phrase: wrapping
  legitimately breaks a phrase at a word boundary, and a test that
  demanded a contiguous substring would be testing the wrap width
  rather than the content.)
* `a_long_diagnosis_does_not_truncate_the_url` — the URL survives a long
  diagnosis.

**Both failed before the fix and pass after it** — the first one failed
twice, which is how the URL defect and the row-height defect were found.

**Version.** `v0.0.1-rc.380` — a patch-level bump. It changes how the
TUI lays out one table. It touches no stored data, no collection schema,
no search behaviour and no wire format.

**Not deployed to the live fleet yet**: this rc is cut after rc.379 was
deployed, and the fleet is on rc.379 until this one is released.

### Top 5 remaining items

1. **The embedder's `idle_release_secs`.** The card cannot host the
   olmocr backend and a resident embedder at once: 14,446 MB free
   against a 13,500 MB headroom is a **946 MB surplus**, smaller than
   the `bench_elements` process another agent runs. Measured again on
   2026-10-22 during the rc.379 deploy: the card had **18,171 MB**
   free and both tiers were healthy — the surplus appears and vanishes
   with the other tenants. `vram_floor_mb` already exists as a key;
   letting the embedder *yield* under contention would give the
   converter the room it needs. This is the other half of issue #10.
2. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client,
   no service, no endpoint. Measured on this corpus, 2026-10-21: it
   moved the expected passage's rank on **4 queries better and 3 worse**
   for a **5× latency increase** (162 → 776 ms median) — and the
   sibling crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model: a serving
   path, a latency budget, and a decision about where it runs.
3. **Source scopes (#4).** The links now name a collection, so a link
   is a capability for one store, and the personal store is read
   through the corpus's storage. What is still missing is the other
   half: a *search* that says which stores it looks in, so a general
   search does not return a game-engine book for "risk factors for
   falls in elderly people" — measured 2026-10-21, the personal
   collection answers that query with *The Base Class for Game Objects
   in SSN67*.
4. **Fix the README drift.** The README describes OCR as "GLM-OCR via
   Ollama" only (the code also has olmOCR, OpenAI-compatible and Cloud
   backends) and omits `crates/openalex-ingest` and the `personal`
   feature from the architecture list. Cheap, and it misleads anyone
   choosing a backend. Two iterations have deferred it as "a docs
   change, not a code change" — that reading is wrong: the rule is one
   *change* per iteration, not one *kind* of change.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.

---

## 2026-10-22-04 — the last site that dropped the diagnosis

**Change.** The release skill's Gate 7 — a regression-class sweep for
the pattern the rc fixed — found one site left: `hs scribe convert`'s
**pre-flight refusal**. It reported the free VRAM but not the headroom
the gate wants, and not **who is holding the card**, so the operator
still had to go and read the server's journal on another host.

```
  before:  scribe server at http://big:7435 reports backend_unavailable:
           13251 MB VRAM free, backend_reachable=true — free GPU memory
           or wait for the current tenant
  after:   scribe server at http://big:7435 reports backend_unavailable:
           13251 MB VRAM free, needs 13500 MB; held by pid=2227527
           7534MB hs-distill-server; pid=2615361 591MB bench_elements,
           backend_reachable=true — free GPU memory or wait for the
           current tenant
```

**The sweep, run over the whole tree.** Three sites mention the
symptom, and after this change **none of them drops the reason**:

| the site | what it reports |
|---|---|
| `crates/hs-mcp/src/main.rs` — the status line | the shortfall, the headroom and the holders (rc.379) |
| `crates/hs/src/status_cmd.rs` — the TUI's Services row | the same, wrapped, with the URL whole (rc.380) |
| `crates/hs/src/scribe_cmd.rs` — the conversion refusal | the same (this rc) |
| `crates/hs-scribe/src/server.rs:222` — the refusal log | already carried `free_vram_mb`, `headroom_mb` and the `holders` |

**Three tests**, in `crates/hs/src/scribe_cmd.rs::refusal_tests`: the
shortfall and the holders are named; a body with no numbers says only
what it knows and **guesses nothing** (no headroom it was not told, no
holders it was not given); an empty holders string is not a holder
list.

**Version.** `v0.0.1-rc.381` — a patch-level bump. It changes one
error message. It touches no stored data, no collection schema, no
search behaviour and no wire format.

**Deployed.** All eight reachable hosts are on rc.381 (`five` is
offline). The refusal was exercised on the live host with the deployed
binary — see the deployment record below.

---

## The rc.381 deployment (2026-10-22, Gates 5-6)

**Gate 5.** `hs upgrade --pre --yes`, in series, on all eight reachable
hosts: `big`, `bmb`, `mac_air`, `big_mac`, `two`, `three`, `one`,
`four`. All now report **rc.381**; `five` is offline. On `big` the
upgrade restarted 9 services and reported `Health: Qdrant: OK` — the
P0-25 timeout did **not** recur this time, so the wait is marginal
rather than reliably wrong.

**Gate 6 — the changed surface, exercised on the live host.** The
deployed scribe server was started with a headroom the card cannot meet
(so its real gate refuses), and the deployed `hs scribe convert` was
pointed at it:

```
  the tier's /health:
    status: backend_unavailable
    free: 17661  headroom: 1048576
    holders: pid=1096335 3386MB hs-distill-server; pid=1091802 2340MB
             hs-scribe-server; pid=1092561 585MB bench_elements

  hs scribe convert, against the refusing tier:
    error: Error: scribe server at http://127.0.0.1:7439 reports
      backend_unavailable: 17661 MB VRAM free, needs 1048576 MB; held by
      pid=1096335 3386MB hs-distill-server; pid=1091802 2340MB
      hs-scribe-server; pid=1092561 585MB bench_elements,
      backend_reachable=true — free GPU memory or wait for the current
      tenant
```

**Before rc.381** that message ended at `17661 MB VRAM free,
backend_reachable=true` — the headroom and the holders were gone, and
the operator had to read the server's journal to find them.

**The live `hs status` on `big` after the rollout:**

```
  ● scribe   http://192.168.1.110:7435            1 converting   0.0.1-rc.381
  ● scribe   http://bmb.local:7433                1 converting   0.0.1-rc.381
  ● distill  http://192.168.1.110:7434            idle (Cuda)  0.0.1-rc.381
  ● qdrant   http://localhost:6334 → academic_papers  1.20.0
```

Both tiers converting, distill on CUDA, Qdrant healthy.

**One P0, already logged: P0-23** — the expired cloud enrollment that
makes `hs status` fail on `bmb`, `mac_air`, `big_mac` and `two`. It
needs an operator-issued invite code; the local services on those hosts
were verified directly instead.

---

## 2026-10-22-05 — the remote document links were 100 % dead

**Change.** The gateway now **refuses to start** when a service it can
route has no backend, and **reports at startup** which of its backends
it cannot reach. The wishlist's second half — *"highlight the text in
the document when all this needs to be served remotely"* — did not work
at all, and nothing in the system said so.

**The evidence.** Ten real links (five documents × source + markdown)
fetched on the live host:

| the path | the result |
|---|---|
| **LAN** | **10 / 10 work** — `206 Partial Content` (the `Range` header honoured) |
| **Remote** | **0 / 10 work** — `403` (Cloudflare's error page) |

**Every remote document link the corpus's agents hand to a user was
dead.** The text fragment — the *highlighting* — was on **100 %** of
the markdown links (80 hits, 8 queries), so the feature was complete
and unreachable at the same time.

**The cause, in two independent host-configuration faults.**

1. **The gateway on `two` had no `docs` route.** Asked directly, it
   answered `502 No backend configured for service: docs` — a request
   the user only makes by clicking a document link. Its
   `cloud.gateway.routes` listed `scribe`, `distill` and `mcp` only.
2. **`big`'s firewall drops TCP 7446.** `ufw`'s default input policy
   is `DROP` and `grep -c 7446 /etc/ufw/user.rules` is **0** — the
   rules allow 7433, 7434, 7435, 7440, 7445, 6333, 6334, 8080, 8081,
   11434 and others, but not the docs port. From `two`,
   `curl http://192.168.1.110:7446/health` → `000` after **7.0 s**
   (the signature of a dropped packet) on **every** host in the fleet.

**The trap that makes the second fault subtle.** The docs service is
**perfectly healthy on its own host**: `/health` answers `200` in
0.76 s on `192.168.1.110:7446` from `big` itself. Every local check
passes while the service is unreachable from everywhere else. **A
self-probe cannot detect it.** Only a probe from another host can, and
the only component that lives on another host and actually proxies is
the **gateway**.

**Why it ranked first.** The retrieval is in excellent shape — the
fixed query set scores **RECALL@10 25/25 = 100 %** (concept 8/8,
literal 10/10, named 7/7), median latency 236 ms, and only 4 % of
first hits are a reference list. So the corpus's answers are right and
its *links* are dead: an agent that finds the right passage and hands
the user a link that shows a Cloudflare error page. That is the worst
possible failure of the wishlist's promise — the answer is right and
the way to check it is broken. It beats the runner-up (cross-encoder
reranking) because reranking improves results that are **already
100 % correct on the query set**, while this breaks the delivery of
every result to every user.

**The change.**

* **The gateway refuses to start** with an unroutable service, naming
  the key to add:

  ```
  Error: /home/ladvien/.home-still/config.yaml: invalid `cloud.gateway` section

  Caused by:
      `cloud.gateway.routes` has no backend for `docs` — every request to it
      would fail with 502. Add the key(s) to `cloud.gateway.routes`, e.g.
      `docs: http://<docs-host>:7446`
  ```

  `GatewayConfig::validate` used to check the routes in one direction
  only: a **configured** unknown service was an error, an
  **unconfigured known** service was silent. This is *"define errors
  out of existence"* applied to a configuration — the request-time
  error is eliminated by refusing the misconfiguration where it can
  still be read.

* **The gateway reports unreachable backends** at startup, from its own
  host. A backend that is down is **not** fatal — one host rebooting
  must not take the other services down with it — so it reports and
  starts:

  ```
  WARN  docs backend http://192.168.1.110:9 is not reachable (connection refused
        — nothing is listening on that port) — every request to `docs` will fail until it is
  WARN  mcp backend http://192.168.1.110:7445 answered HTTP 401 on /health — it is not healthy
  ```

  The reason names the **action**: a timeout means a dropped packet
  (a firewall), a refusal means nothing is listening.

**The tests.** Eight, in `crates/hs-gateway`:

* `config.rs` — the error names the service **and** the key; every
  missing service is named; a route for every routable service is
  valid; and the existing route tests, updated to configure every
  service (they were passing before and must keep passing for the
  right reason).
* `backend_health.rs` — the reason names what to look at; a `401` is a
  different problem from an unreachable host; a healthy backend is
  reachable; the report names the service and the backend.
* The test helper now fills in the services a test does not name: a
  test about `scribe` is not about `docs`, and should not have to say
  so.

**Timings.** The probe adds one HTTP request per backend at startup
(5 s timeout each). On the live host the whole check took **0.26 s**
for four backends. A request's cost is unchanged.

**What was done on the fleet, and what needs the operator.** The
`docs` route was added to `two`'s config (backup at
`~/.home-still/config.yaml.bak.pre-docs-route`) and the gateway
restarted — `Routes: ["docs", "mcp", "distill", "scribe"]`. The
**firewall rule on `big` needs the operator's sudo** and is logged as
**P0-26**:

```bash
sudo ufw allow from 192.168.1.0/24 to any port 7446 proto tcp
```

Until that is done the remote links still 504 — and now the gateway
**says so at startup**, which is the point of this change.

**Version.** `v0.0.1-rc.382` — a patch-level bump. It changes the
gateway's startup validation and adds a startup probe. It touches no
stored data, no collection schema, no search behaviour and no wire
format's meaning.

**Deployed.** All eight reachable hosts are on rc.382 (`five` is
offline). See the deployment record below.

---

## The rc.382 deployment (2026-10-22, Gates 5-6)

**Gate 5 — `hs upgrade --pre --yes`, in series.** All eight reachable
hosts are on **rc.382**: `big`, `two`, `three`, `one`, `four`, `bmb`,
`mac_air`, `big_mac`. `five` is **offline** (`No route to host`).

**P0-25 recurred on `big`**: the upgrade ended with `Error: Timed out
waiting for Qdrant at http://localhost:6333/healthz (90s)`. Qdrant was
**recovering its collections** (15,933 documents across three) and
answered 200 about seven minutes later. The host was healthy
throughout. **This is the third time**: the 90-second wait is shorter
than the recovery the upgrade itself causes, and it will expire on
every rollout until the wait is fixed.

**Gate 6 — the changed surface, on the live fleet.** The gateway on
`two` was restarted with the new binary, and it reported the firewall
gap that has been silently breaking the remote document links:

```
INFO  Routes: ["scribe", "docs", "mcp", "distill"]
WARN  mcp backend http://192.168.1.110:7445 answered HTTP 401 on /health — it is not healthy
WARN  docs backend http://192.168.1.110:7446 is not reachable (timed out
      — the host did not answer (a firewall that drops the port?)) — every
      request to `docs` will fail until it is
```

**That warning is the change working.** Before rc.382 the only symptom
was a `403` in a browser; now the gateway names the service, the
address, the reason and the consequence — and points at the firewall.

**The links after the deployment**, measured over 10 real links:

| the path | the result |
|---|---|
| LAN | **10 / 10 work** (`206 Partial Content`) |
| Remote | **0 / 10 work** (`403`) |

**The remote path still needs the firewall rule on `big`**, which
needs the operator's sudo (`ufw` is not in `big`'s passwordless list).
It is **P0-26**, with the exact command:

```bash
sudo ufw allow from 192.168.1.0/24 to any port 7446 proto tcp
```

Until it is applied the remote links stay dead — and now the fleet
**says so at startup**, which is what this rc bought.

### Top 5 remaining items

1. **The page map: 27.5 % of search hits carry no page.** Measured
   over 80 hits from 8 queries: **72.5 %** of hits have a `page` (so a
   `#page=` fragment on the source link) and **27.5 %** do not, so
   their link opens page 1 of a 30-page paper. The cause is measured:
   the **olmocr backend emits one flat blob** with no page separators,
   so `compute_page_offsets` finds one page for the whole document, and
   `resolve_page_accounting` marks the offsets untrustworthy (a lone
   offset would report page 1 for every chunk). **58.3 %** of the
   corpus's markdown objects have no separator; **45.0 %** of catalog
   rows have a usable page map; **36.7 %** have a page *count* but no
   map. The fix is to recover the byte→page map for a flat blob —
   the PDF's text layer is the obvious source — which is a real piece
   of work, not a config change.
2. **The embedder's `idle_release_secs`.** The card cannot host the
   olmocr backend and a resident embedder at once: 14,446 MB free
   against a 13,500 MB headroom is a **946 MB surplus**, smaller than
   the `bench_elements` process another agent runs. Measured again on
   2026-10-22 during the rc.379 deploy: the card had **18,171 MB**
   free and both tiers were healthy — the surplus appears and vanishes
   with the other tenants. `vram_floor_mb` already exists as a key;
   letting the embedder *yield* under contention would give the
   converter the room it needs.
3. **Cross-encoder reranking.** The GPU blocker has cleared, but the
   reranker is **not in home-still at all**: no code path, no client,
   no service, no endpoint. Measured on this corpus, 2026-10-21: it
   moved the expected passage's rank on **4 queries better and 3 worse**
   for a **5× latency increase** (162 → 776 ms median) — and the
   sibling crate's own M37 measured the same null (+0.78 points, CI
   [−3.14, +4.71]). It needs a *process* as much as a model.
4. **Source scopes (#4), the base for the personal links.** The MCP
   layer has no notion of which source a document came from (academic,
   personal, internet, synthesis, iMessages) and no per-source scope,
   so every tool returns every document. It is the prerequisite for
   personal-document links and their short TTL, and it is what stops a
   personal document leaking through a general search — measured
   2026-10-21, the personal collection answers "risk factors for
   falls in elderly people" with *The Base Class for Game Objects in
   SSN67*.
5. **Decide the async-nats crash-loop** (see
   `needs-decision/async-nats-missed-idle-heartbeat.md`). Recommended
   option A: catch `MissingHeartbeat` and recreate the pull stream
   instead of exiting. Not ours to make — it is a change to a vendored
   crate.

---

## 2026-10-22-06 — olmocr conversions keep their pages

**Change.** The olmocr converter reads olmocr's result document
(`results/output_<hash>.jsonl`) instead of its flattened `.md`, cuts the
text at olmocr's own per-page spans (`attributes.pdf_page_numbers`) and
joins the pages with `join_pages`. The catalog then gets a trustworthy
page map, every chunk a `page`, and every hit's source link `#page=N`.

**Evidence.**

* 27.5 % of search hits (80 hits, 8 queries) carried no `page`, so the
  PDF link opened page 1.
* Seeded 100-row cross-tab of converter × markdown shape: **olmocr 0
  separated / 39 flat**; layout pipeline 25 / 2 (single-page), GLM 7 / 1,
  HTML 0 / 4 (no pages, correct). The flat documents are olmocr's.
* olmocr's pipeline (`build_dolma_document`) records exact spans; the
  `.md` it writes is the same text joined by `"\n"`. A real 6-page run
  confirmed it: 1 result document, contiguous spans, `.md` byte-identical
  to `text` with 0 separators, and spans in **characters** (33,420 chars,
  33,602 bytes).

**Before / after** (same real olmocr output, real server binary over
HTTP; real chunker on the result):

| | rc.382 | rc.384 |
|---|---|---|
| pages in the markdown | 1 | **6** |
| pages equal to olmocr's spans | 0/6 | **6/6** |
| catalog page map trusted | no | **yes** |
| chunks with a `page` | **0/10** | **11/11** |
| RECALL@10 (query set) | 25/25 | 25/25 |

**Why it ranked first.** It was the top remaining item and the
measured cause of 27.5 % of hits opening at the wrong page — the
wishlist's "link the user can view" delivered at the wrong place. It
beat the runner-up (the embedder yielding VRAM, now P0-27) because that
one is a backend-capacity problem whose fix spans two config files on a
shared box, while this was a self-contained converter defect with a
single authoritative source of truth.

**Version.** `v0.0.1-rc.384` — patch: converter output for new
documents only; no schema, wire or search change. **rc.383 was
withdrawn unpublished**: its CI gate failed on a pre-existing ETXTBSY
race in hs-scribe's stand-in-CLI tests, reproduced inside the runner's
image (4/60) and fixed (0/60) — `test_support::install_stand_in`.
Commits `7838894` (the change), `d4dc178` (the race).

**Scope.** Existing flat olmocr documents keep no pages until
re-converted (no corpus rebuilds). Not deployed (`hs upgrade` is not run
by this loop).

**Found on the way (logged).** llama-swap was wedged (restart cleared
it); **P0-27** the olmocr tier's VRAM gate admits at 13,500 MB while vLLM
needs ~15,718 MB (vLLM's own log: 14.53 GiB free < 15.35 GiB wanted) —
the measured cause of issue #10, now commented there; **P1-27** a cold
start fails whole documents (llama-swap proxies to a vLLM not yet
listening; 6/6 pages failed in ~20 s); **P1-28** `hs paper download
<doi>` searches for the DOI and downloads 10 papers. P0-26 (firewall rule
for 7446 on `big`) still needs the operator.

### Top 5 remaining items

1. **P0-27: the olmocr VRAM gate is below vLLM's requirement.** Derive
   the headroom from `--gpu-memory-utilization` × card memory, or refuse
   a configured headroom below it. Today the tier admits documents it
   cannot convert whenever 13.5–15.7 GB is free.
2. **P1-27: olmocr cold starts fail whole documents.** Wait for
   llama-swap to report the model `ready` before invoking olmocr (the
   server already probes `/running`).
3. **P0-26: the docs port firewall rule on `big`** (operator, sudo):
   every remote document link is still dead.
4. **Re-convert flat olmocr documents** (needs a decision: it is a
   targeted re-conversion of ~half the corpus, which this loop may not
   start on its own). Every page-less hit today is one of these.
5. **Cross-encoder reranking.** Unchanged: 4 better / 3 worse on the
   query set for 5× latency; needs a serving decision.

## 2026-10-23-01 — the repetition detector stopped dropping documents

**Change.** The scribe server's streaming repetition detector fires only
on a **consecutive** loop: the stream's tail is ≥ 3 back-to-back copies
of one unit of ≤ 128 words, spanning ≥ 32 words. It used to count n-grams
anywhere in a 256-word window (a 4–6-gram three times, a trigram five, a
bigram ten), which ordinary prose does.

**Evidence.**

* In the 24 h before the change, **176/176** documents the two scribe
  watchers received ended `terminating (will not redeliver)`; **154** as
  `gapped_conversion` from bmb's glm_ocr tier. The gaps were the
  detector's aborts (`4-gram-cycle` 2,655, `trigram-cycle` 556 in bmb's
  log), and gapped is permanent.
* Replaying the detector over 200 seeded **accepted** documents, it
  aborted ≥ 1 block in 96.4 % of olmocr documents (text it never vetted),
  90.3 % of scribe-vlm, 72.1 % of glm_ocr. The blocks were affiliation
  lists, citation runs, a key phrase said three times.
* Research: Xu et al. 2022 (consecutive sentence repetition is ~0.02 % of
  human text, and repetition self-reinforces), Nougat (OCR decoders repeat
  the last sentence), Holtzman et al. 2020; all in the corpus.

**Before / after** (same data; `runs/2026-10-23-01/eval.md`):

| | rc.384 | rc.385 |
|---|---|---|
| glm_ocr docs with an abort on accepted text | 72.1 % | **0 %** |
| olmocr docs with an abort on accepted text | 96.4 % | **3.6 %** (mostly real stored loops) |
| recall, 2-word loop from real text | 88.7 % | **98.8 %** |
| recall, last sentence repeated | 88.3 % | **95.6 %** |
| recall, paragraph replayed | 88.4 % | **94.3 %** |
| `10.1016_j.eurpsy.2012.03.001` (dropped in prod, 6 gaps) | refused, 6 gaps | **stored**, 6/6 pages |
| `10.1016_j.eplepsyres.2007.12.015` (dropped in prod, 14 gaps) | refused, 14 gaps | **stored**, 6/6 pages |
| detector cost per word | 439 ns | **211 ns** |
| RECALL@10 (query set) | 25/25 | 25/25 |

The old server reproduced production's gap counts exactly (6, 14), so the
end-to-end is the production failure. The new outputs contain no loop
(at most 2 back-to-back copies of anything). The cost is aborting a real
loop later: ~29 words into a 2-word loop instead of ~6.

**Why it ranked first.** It was destroying essentially every document
the glm_ocr tier converted, permanently, which is rank 1 (data loss). The
runner-up, P0-27 (the olmocr tier's VRAM gate), stops one tier, and that
tier's documents then escalate into this one, where they were dropped
too, so this fix is the one that stops the loss.

**Version.** `v0.0.1-rc.385` — patch: changes which streams the detector
aborts; no wire, schema or config change (`LoopReason` keeps its form).
Release workflow green (Gate on big, macOS and Windows; 5 builds; Docker),
33 assets; downloaded `hs` (x86-64) and `hs-scribe-server` (arm64 Mach-O)
pass their checksums and carry the rc.385 marker. Commit `621afa6`
(change), `c9f8f99` (the `/build-loop` command).

**Fleet gap.** big (both scribe tiers, `hs`), bmb and two run **rc.382**.
The fix takes effect only on a scribe server that runs it; **bmb** is the
one that matters. Not deployed (`hs upgrade` is the operator's call).

**Found on the way.**

* **Conversion has stored nothing for 14 h** (an operator's status check,
  2026-10-23): 274 terminal, 266 gapped, 0 stored. Two causes: the bug
  fixed here (bmb), and **P0-27** on big — vLLM exits at startup
  (`Engine core initialization failed`, 14,077 MB free < ~15,718 MB
  needed, gate at 13,500 MB), so olmocr runs cycle against a dead backend
  and escalate into bmb's drops. A scribe restart would not help. Written
  up in `needs-decision/gapped-drops-and-dead-olmocr.md`.
* BACKLOG **P0-28** (re-ingest the dropped documents), **P1-29** (verbatim
  loops already stored in 24 of 200 sampled documents), **P1-30** (bmb's
  autotune agent crash-loops; its scribe log is 1.1 GB).
* The test gate: under a bare `unshare -rn` the loopback interface is
  down, and 72 tests that bind a local server fail with `os error 101`.
  Run it as `unshare -rn sh -c 'ip link set lo up && cargo test …'`.
* rustdoc has 30 pre-existing warnings, so `-D warnings` fails on code
  this change does not touch.

### Top 5 remaining items

1. **Deploy rc.385 to bmb** (operator: `hs upgrade --pre --yes` there).
   Until then ~19 documents an hour are dropped as gapped.
2. **P0-27: the olmocr tier cannot start.** Derive the VRAM gate from
   vLLM's real requirement (utilization × card memory) so the two cannot
   drift; interim config: gate at ~15,800 MB, or utilization ~0.55.
3. **P0-28: recover the documents dropped as gapped** (needs a decision:
   a dry-run enumeration by catalog stamp, then re-publish, after 1).
4. **P0-26: the docs port firewall rule on big** (operator, sudo): every
   remote document link is still dead.
5. **P1-27: olmocr cold starts fail whole documents** — wait for
   llama-swap to report the model ready before invoking olmocr.

## 2026-10-23-02 — failed conversions are reported by what a retry can do

**Change.** `hs status` no longer calls every `conversion_failed` stamp a
"Corrupted PDF". Each stamp's reason maps, through the chain's own
classification (`FailureCode::class`), to **retryable** (a backend failed
on the document), **bad source** (it can never convert) or
**unclassified**. The dashboard's "Failed, retry" row shows the retryable
count, the recovery command and the top reason; the conversion progress
leaves out only bad sources; `system_status` carries the breakdown and
`catalog_list` each row's `{reason, kind}`. `hs pipeline
reconvert-failed` re-queues retryable stamps by default, and `--reason`
selects exact reasons.

**Evidence.**

* The status said **"Corrupted PDFs 2208"**; an operator's check read it
  as a static count of bad files. On a read-only copy of the catalog, the
  2,208 stamps are **2,053 retryable** (1,583 `gapped_conversion`, 432
  `vlm_transport_error`, 21 `vlm_repetition_loop`, 17 untyped
  escalations) and **155 bad sources**.
* The gapped stamps grew 59 → 187 → 109 → 357 → **472** per day
  (10-17 … 10-22), 397 more by noon 10-23, under a label that said the
  input was broken. The progress bar subtracted them: it read **100.0 %**.
* Literature (in the corpus): *Enterprise Integration Patterns* keeps
  unprocessable messages and undeliverable ones on separate, monitored
  channels; Avižienis et al. make detectability — signaling a failure to
  someone who can act — a dimension of every failure mode.

**Before / after** (same live catalog; `runs/2026-10-23-02/eval.md`):

| | rc.382 | rc.386 |
|---|---|---|
| failure row | "Corrupted PDFs 2208" | "Failed, retry 2053" + `hs pipeline reconvert-failed --dry-run` + "gapped_conversion 1583 · 155 bad source" |
| conversion progress | 100.0 % (capped) | **88.9 %** |
| `reconvert-failed` default selection | 2,208 (155 bad sources re-queued for nothing) | **2,053** (155 left) |
| a typo'd `--reason gapped` | n/a | exit 1, names the reasons present |
| `hs status` latency (median, n=5) | 8.77 s | 8.27 s |
| RECALL@10 (query set) | 25/25 | 25/25 |

The live split equals the offline prediction (2,053 / 155 / 0). The
first cut of the row, at 110 columns, truncated to `hs pipeline
reconvert-failed` — the live bulk command without `--dry-run`; the row
now holds the command alone and a test pins both widths.

**Why it ranked first.** The live data loss (rank 1) is already fixed in
rc.385 and needs only the bmb deploy, which no code can replace; what
remained was the reason a week of it went unnoticed and the recovery
tool that could not be aimed at it. It beat P0-27 because that outage is
now diagnosed down to a one-line operator action (below).

**Version.** `v0.0.1-rc.386` — minor: `system_status.pipeline.
corrupted_pdfs` is replaced by `failed_conversions`, `catalog_list`'s
`corrupted` by `conversion_failed`, and `reconvert-failed`'s default
selection narrows. `hs status` needs the CLI and the MCP server on rc.386
(an older server leaves the row unknown). Release workflow green (Gate on big, macOS and Windows; 5 builds; Docker), 33 assets; downloaded `hs` (x86-64) and `hs-mcp` (x86-64 and arm64 Mach-O) pass their checksums, carry the rc.386 marker and report `0.0.1-rc.386`. Commit `e1e52ea`.

**Fleet gap.** big, bmb and two run **rc.382** (four releases behind):
neither rc.385's fix nor this one is live.

**Found on the way.**

* **What starves the olmocr tier (P0-27):** big's `hs-serve-scribe`
  (`:7433`) — disabled, not in the pool, 0 conversions — has run since an
  accidental start on 2026-10-02 and holds **2,340 MiB**; without it
  16,417 MB would be free, above vLLM's ~15,718 MB. Stopping it is the
  operator's one-line fix (`needs-decision/gapped-drops-and-dead-olmocr.md`,
  C.0).
* `vlm_transport_error`: 423 of 432 stamps came from one incident on
  2026-10-17; retryable, recovered by the same command.

### Top 5 remaining items

1. **Deploy rc.385/386 to bmb, and stop big's idle `:7433` scribe**
   (operator). Until bmb runs rc.385, ~19 documents an hour are dropped
   as gapped; until `:7433` stops, the olmocr tier cannot start.
2. **P0-28: recover the 2,053 retryable documents** after 1 —
   `hs pipeline reconvert-failed --dry-run`, then staged runs.
3. **P0-27: derive the olmocr VRAM gate from vLLM's real requirement**,
   so the gate refuses honestly instead of admitting work the backend
   cannot start.
4. **P0-26: the docs port firewall rule on big** (operator, sudo):
   remote document links are still dead.
5. **P1-27: olmocr cold starts fail whole documents** — wait for
   llama-swap to report the model ready before invoking olmocr.

## 2026-10-23-03 — catch-up never re-queues a failed conversion

**Change.** `hs pipeline catch-up` skips documents stamped
`conversion_failed`, reports them by kind and points at `hs pipeline
reconvert-failed` (which selects by what a retry can do and clears the
stamp first). `hs status`'s Stalled counts only unconverted documents with
no failure stamp — what catch-up queues; `pipeline_drift` still counts
all of them. Without the catalog, catch-up fails and Stalled is unknown.

**Evidence.**

* All **87** documents the scribe watchers terminated in a 6 h window
  were already stamped failed that morning (attempts 2–8; one
  `pdf_parse_error` at 40). Nothing new was being converted.
* The `PAPERS` work queue (FIFO) was read message by message without
  consuming it: of 1,197 pending, **1,086** came from one catch-up run on
  2026-10-21 (published in 13 s) — 1,010 retryable failures, 60 sources
  that can never convert, 13 already converted, **3** real work. 101 new
  downloads queued after it had waited two days.
* `hs status` showed Stalled 888 — all stamped failures — with the note
  "run `hs pipeline catch-up`", the command that caused the backlog.
* Literature (in the corpus): *Enterprise Integration Patterns* — a
  message that makes its recipient fail "would just fail right away
  again", so it belongs on its own channel; Bass et al. — FIFO assigns
  priority by arrival alone, so a bulk re-publish outranks every later
  arrival.

**Before / after** (same live store, dry runs only;
`runs/2026-10-23-03/eval.md`):

| | rc.382 | rc.387 |
|---|---|---|
| catch-up would republish | 2,152 | **144** |
| failed conversions skipped (and reported) | 0 | **2,008** (1,888 retryable, 120 not) |
| Stalled | 888 | **0** |
| `pipeline_drift` | 2,144 | 2,145 (one new download) |
| catch-up `--dry-run` wall time | 1.7 s | 6.8 s (reads the catalog) |
| RECALL@10 (query set) | 25/25 | 25/25 |

Stalled 0 is true: the queue holds exactly the 144 unstamped,
unconverted stems catch-up would queue.

**Why it ranked first.** It was the measured reason the scribe tiers
were converting nothing new (rank 1: documents stuck): known failures
were being re-fed ahead of new downloads, and the dashboard told the
operator to do it again. It beat P0-27 (olmocr VRAM gate) because that
outage is down to an operator action (stop big's idle `:7433` scribe).

**Version.** `v0.0.1-rc.387` — catch-up's selection narrows and Stalled
changes meaning; no wire or schema change beyond rc.386. Release workflow green (Gate on big, macOS and Windows; 5 builds; Docker), 33 assets; downloaded `hs` (x86-64, arm64 Mach-O) and `hs-mcp` (x86-64) pass their checksums, carry the rc.387 marker and report `0.0.1-rc.387`. Commit `c8332b5`.

**Fleet gap.** big, bmb and two run **rc.382** — five releases behind
(rc.383 was withdrawn). None of rc.385–387 is live.

**Found on the way.** The 1,070 failure events already queued need no
action beyond the rc.385 deploy on bmb; re-publishing them again would
double them (BACKLOG P0-28 note). Gates: workspace 1,581 / hs-scribe 378 /
hs-common (all features) 374 tests pass; rustdoc unchanged at 30 warnings.

### Top 5 remaining items

1. **Operator: upgrade bmb (rc.385 stops the gapped drops), stop big's
   idle `hs-serve-scribe`** (frees the olmocr tier), then upgrade big and
   two.
2. **P0-28: recover the retryable documents** — the 1,010 already queued
   retry on their own after 1; then `hs pipeline reconvert-failed
   --dry-run` for what is still stamped.
3. **P0-27: derive the olmocr VRAM gate from vLLM's real requirement.**
4. **P0-26: the docs port firewall rule on big** (operator, sudo).
5. **P1-27: olmocr cold starts fail whole documents.**

## 2026-10-23-04 — `hs upgrade` leaves an unchanged, healthy Qdrant running

**First pick, blocked: the embedder's VRAM.** The olmocr tier has not
converted since 2026-10-22 22:01 UTC. The idle distill embedder held
8,510 MiB (6,462 a day earlier), leaving 13,137 MiB free against vLLM's
~15,718 — stopping the idle `:7433` scribe is no longer enough. Measured
with the real embedder on 200 real documents: the CUDA arena grows
3,386 → 7,482 MiB on the first document (+4,096, a power-of-two step) and
never shrinks; `kSameAsRequested` is worse (13,056); a hard
`gpu_mem_limit` fails batches. The fix that would work (per-run arena
shrinkage) needs a fastembed change. Written up with measured options in
`needs-decision/gpu-budget-olmocr-vs-embedder.md`; BACKLOG **P0-29**.

**Change.** After pulling, `hs upgrade` recreates a compose service only
if its containers do not already run the images their tags name, or the
service does not answer its health URL; otherwise it leaves them
running. Any probe failure recreates, as before.

**Evidence.** Every upgrade on the GPU host stopped and restarted Qdrant
with an unchanged image. Measured on a throwaway Qdrant (reflink copy of
the live 7.3 GB storage), the exact old sequence: search outage **44.8 s**
and **67.0 s**. In production on 2026-10-22: ~7 minutes, after which the
90 s wait failed a healthy upgrade (P0-25). Literature (in the corpus):
Bass et al. — rolling upgrade, and *escalating restart* "minimizing the
level of service affected".

**Before / after.**

| | rc.387 | rc.388 |
|---|---|---|
| Qdrant outage per upgrade, image unchanged | 44.8–67.0 s (copy), ~7 min (production) | **0 s** (left running) |
| false "Timed out waiting for Qdrant" in that case | yes (P0-25) | no — never stopped |
| changed image / stale or missing container / unhealthy | recreate | recreate (unchanged) |

The live host, probed read-only with the same commands: container on
image `8a80c550…`, tag names `8a80c550…`, `/healthz` 200 → left running.
Not done for time: an end-to-end run of the real function against the
throwaway Qdrant; covered by stand-in tests of both paths.

**Gates.** fmt; clippy ×3; workspace tests 1,585 passed / 0 failed;
`hs` 237. Version bake `hs 0.0.1-rc.388` with marker.

**Version.** `v0.0.1-rc.388`, commit `7346514`. Tagged and pushed; the release workflow (run `40016609189`) was still running when this was written, so the artifacts are **not yet verified** — check `gh run view 40016609189 -R home-still/home` and the downloaded `hs` before deploying.

**Fleet gap.** big, bmb and two run **rc.382** — six releases behind.

### Top 5 remaining items

1. **Operator: upgrade bmb** (rc.385 stops the gapped drops), then big
   and two.
2. **Operator: free the card for olmocr** — restart distill and stop
   `:7433` (→ ~16.5 GB free), then set `embedding.idle_release_secs`
   (P0-29).
3. **P0-28: recover the retryable documents** after 1 (`hs pipeline
   reconvert-failed --dry-run`).
4. **P0-25 remainder:** when Qdrant's image did change, the 90 s wait
   should tell "recovering" from "down".
5. **P0-27 / P1-27:** derive the olmocr VRAM gate from vLLM's
   requirement; wait for a cold model before invoking olmocr.
