# Run 2026-10-20-02 — reference-list chunks in search results

## Problem, measured

The previous iteration took recall@10 from 80 % to 100 % on the fixed
query set (`docs/loop/queries.yaml`, 25 queries). But recall only says
the expected string is *somewhere* in the top 10. Looking at what a
user actually sees, the top of the list is often a **bibliography
entry** — a chunk that is a list of citations to other papers, not a
passage about the query.

Measured on the live server over the 438,863-chunk corpus, with a
detector for citation lines (a line carrying a `doi:`, a parenthesised
year after author names, `et al.`, a page range, a volume, or a
numbered-list marker):

| population | reference-like share |
|---|---|
| top-10 of each of the 25 fixed queries | 58/250 = **23.2 %** |
| top-3 of each of the 25 fixed queries | 18/75 = **24.0 %** |
| a random sample of 300 corpus chunks | 31/300 = **10.3 %** |

The queries are where it is worst, because a reference list *is* about
the topic: a chunk listing "Berkenbosch F, van Vant J, Aubert A, et al.
Brain-derived neurotrophic factor…" matches "hippocampal neurogenesis
and adult memory" perfectly well in a dense embedding. The embedder
cannot tell a citation from a discussion, because semantically they are
the same thing.

**What the user sees.** For `hs distill search "Gazzaniga split brain"`:

```
1.  [no title]  ice and rats. Neuropharmacology 62, 1191–1203. doi: 10.1016/…
2.  [no title]  anges in patients with a first episode of major depression…
3.  The magical number 4 in short-term memory
4.  [no title]  and brain abnormalities is disease specific…
```

The two best matches are reference lists. The one chunk that is about
the topic is third. The user asked a question and got a bibliography.

## Research

### The project already detects bibliographies — and throws the flag away

`hs-scribe` detects bibliography pages at conversion time:
`crates/hs-scribe/src/postprocess.rs::is_bibliography_page` (the layout
model's `reference` / `reference_content` classes), and uses it in
`qc_verdict` to relax the truncation budget for reference pages
("a bibliography legitimately repeats, so the repetition count must not
reject it"). `crates/hs-scribe/src/pipeline/processor.rs` carries the
per-page flag into the QC layer.

That flag **never reaches the distill side**: `grep -rn "bibliograph"
crates/hs-distill/src/ hs-common/src/catalog.rs` finds nothing, and
`chunk_payload` (`crates/hs-distill/src/qdrant.rs:341`) stores no
bibliography marker. The knowledge that a page is a reference list is
produced at the right place and lost at the seam.

Using the layout model's own verdict would be the most accurate signal
available. It is also the one that cannot be applied now: the flag
exists only for documents converted *after* it was added, the corpus
predates it, and re-converting the corpus is out of scope (and would
need the GPU this host shares with another agent's job).

### What the literature in this corpus says

Searching the corpus for the technique:

- **"Sparse-dense complementarity"** (`10.48550_arxiv.2604.02431`,
  in the corpus) — lexical and semantic retrieval retrieve different
  documents, and fusing their *ranks* beats either alone. This is what
  the previous iteration built.
- **Cormack, Clarke & Buettcher (2009)**, "Reciprocal rank fusion
  outperforms Condorcet and individual rank learning methods"
  (`10.1145_1571941.1572114`, in the corpus) — RRF combines rankings
  rather than scores, so channels with incomparable scales can be
  merged. The same paper's framing is the reason a *rank* adjustment
  is safe here too: a demotion that changes a hit's position changes
  its rank, and RRF is a function of ranks.
- **RocketQAv2** (Ren et al. 2021, `10.18653_v1_2021.emnlp-main.224`,
  in the corpus) — cross-encoder reranking is "the highest-leverage
  stage" over a fused candidate pool: MS MARCO MRR@10 goes 18.7 → 36.5.

RocketQAv2 is the ceiling. A cross-encoder scores a query *against* a
passage and can tell a bibliography entry from a discussion of the
topic. It is also the reason this iteration is not the cross-encoder
iteration: it needs a second GPU service (`bge-reranker-v2-m3`), and
the 3090 on `big` is currently shared with another agent's job. The
next item in the loop's backlog, and the right one to do when the card
is free.

### Techniques considered

**A. Cross-encoder rerank.** The right answer, the proven one, and
already ours (our own `myelin` crate runs `bge-reranker-v2-m3`).
Blocked on a GPU service, and on being able to re-rank *more* than the
10 candidates the current fusion returns. Deferred, not skipped — it is
the top of the next backlog.

**B. Re-index with a bibliography flag.** Most accurate (the layout
model already decides), but it means re-converting or re-embedding the
corpus, which the brief forbids and which would need the GPU.

**C. A structural detector at search time.** A citation line is
recognisable by shape: `doi:`, a parenthesised year after author
names, `et al.`, a page range, a volume, a numbered-list marker. No
model, no GPU, no re-embed. It catches 10.3 % of the corpus and 23.2 %
of query results.

This iteration does C.

### The threshold matters, and it is measurable

Sampling 300 corpus chunks:

| bib fraction | share | meaning |
|---|---|---|
| > 0.5 | 10.3 % | a list of citations — never the answer to "tell me about X" |
| 0.2 – 0.5 | 14.0 % | mixed: a citation and real prose in one chunk |
| ≤ 0.2 | 75.7 % | prose |

A threshold at 0.5 demotes only the pure lists and keeps every mixed
chunk. A threshold at 0.2 would destroy 14 % of the corpus — chunks
that discuss a topic and cite it — which is exactly the content a
reader wants. The threshold is therefore 0.5, and the detector is
deliberately conservative: it must not fire on prose that merely
contains a year in brackets.

## Technique chosen

**Demote reference-list chunks in the fused result.** A chunk whose
citation-line share exceeds 0.5 is not *deleted* — it is pushed below
the prose chunks, keeping its RRF score and its place in the ranking
maths. Deleting results would silently reduce recall; demoting
preserves every answer while moving the useless ones out of the way.

The detector runs where the reader sees the effect: on the way out of
`/search`, over the fused list.

**Why demote and not delete.** A user asking "give me the bibliography of
this paper" *wants* the reference list. Deleting it would make that
query unanswerable. Demoting it means the reference list is still there,
just not ahead of the passage that actually discusses the topic.

**Why not at index time.** A search-time demotion needs no corpus
change, no re-embed, and no GPU, and it applies to the 438,863 chunks
already indexed. An index-time filter would only help documents
indexed after it shipped, and would have to be re-run to cover the rest.
The cost of search-time is one pass over the (at most `limit`) results,
which is negligible next to the two Qdrant round trips a search already
makes.

## Expected effect

- The reference-like share of the top 3 falls from 18/75 to 0/75 over
  the fixed query set.
- **Recall must not fall.** Measured: demotion moves the expected
  passage out of the top 10 for **0** of the 25 queries.
- Latency: unchanged within measurement noise (the demotion is a
  re-order of an already-materialised list).

## Constraint compliance

- No corpus rebuild, no purge, no re-embed, no durable-consumer changes.
- No new dependency: the detector is ordinary string inspection.
- The dense and lexical channels are unchanged; this is a presentation
  rule on their fused result, not a third channel.
- Nothing is deleted — a demoted hit is still returned, still scored,
  and still reachable by raising `limit`.
