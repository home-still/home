# Run 2026-10-20-01 — hybrid (dense + keyword) retrieval

## Problem, measured

`hs distill search` is dense-only: one bge-m3 vector per chunk, Qdrant cosine
search, no lexical channel, no reranking. The fixed query set
(`docs/loop/queries.yaml`, 18 queries) scores:

```
RECALL@10: 10/18 = 55.6%
  concept  7/8  = 87.5%
  literal  1/5  = 20.0%
  named    2/5  = 40.0%
  median latency 118 ms
```

The split is the finding. Concept queries — "how does a transformer attention
mechanism work" — are found. Queries containing a **specific term** are not.

Searching `Hashimoto thyroiditis euthyroid` returns passages about
thyrotoxicosis, thyroid scoring in a heart-failure risk table, and bipolar
mania. Not one result is about Hashimoto's disease. The embedder matched the
generic token "thyroid" and lost the specific one.

The corpus is not at fault. `grep` over the markdown shows the answer is
already there:

| term | markdown files containing it |
|---|---|
| Hashimoto | 111 |
| Kandel | 27 |
| Bergmann | 25 |
| Gazzaniga | 12 |
| epiphyseal | 4 |
| uveitis | 6 |

This is a retrieval failure, not a data gap: 8 of 18 queries have the
expected passage sitting in the corpus and the search never surfaces it. A
user asking "what does this corpus say about Hashimoto's?" gets confidently
wrong answers — the worst outcome the tool has, because it is silent.

## Research

### Sources read from this corpus (via `hs distill search`)

- **Cormack, Clarke & Buettcher (2009), "Reciprocal rank fusion outperforms
  Condorcet and individual rank learning methods"** —
  `10.1145_1571941.1572114` (in the corpus; read directly). RRF combines
  rankings by `score(d) = Σ_lists 1/(k + rank_list(d))`, so a document that is
  good under one ranking is not buried by a rival that is better under
  another. It needs no score calibration between the two channels.
- **"Sparse-dense complementarity"** (`10.48550_arxiv.2604.02431`, in the
  corpus). "The complementarity of lexical and semantic retrieval is
  well-established (Lin et al., 2021). Reciprocal Rank Fusion (Cormack et al.,
  2009) provi[des]…" — lexical and dense retrieve different documents, and
  fusing their *ranks* beats either alone.
- **Chen et al. (2020), "Dense Passage Retrieval for Open-Domain Question
  Answering"** — `10.18653_v1_2020.emnlp-main.550` (in the corpus). The
  dense baseline this project inherits, and the paper that reports BM25
  ("parameters b = 0.4 … k1 = 0.9") as a strong sparse baseline.
- **Ren et al. (2021), "RocketQAv2: A Joint Training Method for Dense Passage
  Retrieval and Passage Re-ranking"** — `10.18653_v1_2021.emnlp-main.224` (in
  the corpus). Establishes cross-encoder reranking as the highest-leverage
  stage over a fused candidate pool.

### The sibling crate we own

`home-still/myelin` (`myelin-r4`, MIT, "Copyright (c) 2026 Home Still") is
our own long-term-memory crate and already solves this exact problem for
agent memory. Its measured numbers (`docs/FEATURES.md` §RRF fusion):

> "BM25 alone scores 0.8666 and dense alone 0.7314 … M4 measured k=1 at
> 0.8815 dev recall@6 vs k=60 at 0.8252."

and its implementation is directly reusable:

- `myelin-core/src/pipeline/fuse.rs::rrf(&[RankedList], k)` — RRF over id
  lists, `k` configurable, tested against a recorded Qdrant series.
- `myelin-core/src/store/qdrant.rs` — Qdrant **native sparse vectors** with
  `Modifier::Idf` for the BM25 channel, and a named dense vector alongside.

Two things myelin does that home-still cannot copy directly:

1. **myelin's BM25 is a Qdrant sparse vector**, which means every stored
   point needs a sparse vector alongside its dense one. home-still's corpus
   is 438,864 chunks and re-embedding it is out of scope for this iteration
   (and the brief forbids rebuilding the corpus).
2. **myelin's reranker is a separate service** (`bge-reranker-v2-m3` on
   :5813) that is not running on this host and would add a second GPU
   dependency to a machine another agent is already using.

So: take myelin's *fusion* (RRF) and its *evidence* (dense and lexical
retrieve different documents), but implement the lexical channel in a way
that needs **no re-embedding**.

## Technique chosen

**Dense (existing) + a lexical candidate channel over the existing payload,
fused with RRF.**

The lexical channel uses Qdrant's **payload text index** on `chunk_text`,
which home-still already stores in every point's payload. Verified on the
live collection (Qdrant 1.19.2):

- `PUT /collections/academic_papers/index` with
  `{"field_name":"chunk_text","field_schema":{"type":"text","tokenizer":"word"}}`
  is accepted and builds a text index.
- A filter `{"key":"chunk_text","match":{"text":"Hashimoto"}}` selects
  matching chunks in **18–29 ms** over 438k points.
- The match is **AND** over the words in the query (`"Hashimoto thyroiditis"`
  matched only the chunk containing both; `"Hashimoto nonexistentword"`
  matched nothing), so a multi-word query narrows rather than widens.
- Candidate counts for the missed queries: Hashimoto 779 chunks, Bergmann
  198, Kandel 117, Gazzaniga 30, uveitis 9, epiphyseal 6.

The dense channel stays exactly as it is. The two are combined with RRF
(k = 1, myelin's measured value, not Cormack's default 60) over the two
ranked id lists.

**Why this beats the alternatives:**

- *Sparse vectors (myelin's approach)*: best quality, but needs a full
  re-embed of 438k chunks. Forbidden here, and it would put the GPU and the
  corpus at risk for a retrieval change.
- *Cross-encoder reranking alone*: highest ceiling, but needs a second GPU
  service that is not running, and myelin measures it at +579 ms/query. It
  is the natural **next** step once a lexical channel exists to feed it.
- *Keeping dense-only*: leaves 8 of 18 queries silently wrong.

## Expected effect

- `literal` and `named` recall should rise from 20%/40% toward the ~87%
  the concept group already reaches, because the expected passages are
  literally in the corpus and the lexical channel finds them.
- `concept` recall must not fall: RRF keeps a document that the dense
  channel ranks first, and the dense list is one of the two inputs.
- Latency: the lexical channel is one extra Qdrant call at ~20 ms on top of
  the existing ~120 ms dense search.

## Constraint compliance

- No corpus rebuild, no purge, no re-embed, no durable-consumer changes.
- The dense path stays the primary one; the lexical channel is an addition
  to the same single search route, not a second way to search.
- If the lexical channel cannot be built (no text index, or a Qdrant that
  rejects it), the search must fail loudly or say so — not silently fall
  back to a degraded dense-only result.
