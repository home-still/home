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

1. **Cross-encoder reranking.** The GPU blocker has cleared, but the
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
