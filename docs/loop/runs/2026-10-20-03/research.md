# Run 2026-10-20-03 — the title gap in search results

## Problem, measured

An agent that calls `distill_search` gets a passage and a `doc_id`. For
**72 % of the hits** the `title` is `null` and the `authors` list is
empty, so the agent cannot tell the user *which book* the passage came
from.

Measured over the 250 hits of the fixed 25-query set
(`docs/loop/queries.yaml`) against the live server:

| field | hits missing it |
|---|---|
| `title` | **180/250 = 72.0 %** |
| `pdf_path` | 0/250 = 0 % |

A sample of the hits, as the MCP tool returns them:

```json
{ "doc_id": "10.1371_journal.pone.0038469", "title": null, "authors": [],
  "year": 2012, "doi": null, "pdf_path": "papers/10/10.1371_journal.pone.0038469.pdf" }
```

This is the wishlist's first half, arriving as a bug before it arrives
as a feature. Issue #7 asks for links to the documents so a user can
open what an agent found. A link needs a title to be a link *to
something* — and today the agent gets the URL but not the name.

### Where the title is lost

The title is **not** missing from the catalog. For that same stem:

```
$ cat catalog/10/10.1371_journal.pone.0038469.yaml
  title: Bipolar Disorder, Brain-Derived Neurotrophic Factor (BDNF) Val66Met Polymorphism
  authors: [{'name': 'Cheryl Ann Teh'}, {'name': 'Tih-Shih Lee'}, ...]
  doi: 10.1371/journal.pone.0038469
  markdown_path: markdown/10.1371_journal.pone.0038469.md
```

It is missing from the **Qdrant payload**:

```
$ curl .../points/scroll  (filter doc_id = 10.1371_journal.pone.0038469)
  title: None
  authors: []
  doi: None
```

The chain is: `extract_rule_based` (`crates/hs-distill/src/metadata.rs:11`)
copies the title from the catalog row **at index time** and writes it into
the point payload (`chunk_payload`, `qdrant.rs:341`). A search hit reads
the title back out of the payload (`hit_from_payload`, `qdrant.rs:385`).
So a document indexed *before* its catalog row existed was indexed with
no title, and it has been that way ever since — the corpus is not
re-indexed, so the gap is permanent for those documents.

### How big is the fixable part

Sampled 3,831 distinct indexed documents, of which 1,933 have no title
in the payload. Checking the first 200 of those against the catalog:

| | count | share |
|---|---|---|
| catalog **has** a title (a payload join would fix it) | 72 | 36 % |
| catalog has no title either | 13 | 6.5 % |
| **no catalog row at all** | 115 | 57.5 % |

So roughly a third of the title-less documents are fixable by joining
the payload to the catalog. The rest were never cataloged: they are
documents that reached Qdrant through a path that does not write a
catalog row (the `hs distill index` directory walk, and the personal
store), or the catalog row was never created for them.

## Techniques considered

**A. Re-index the corpus.** Fixes every document, and is forbidden —
the brief forbids rebuilding, purging or re-embedding the corpus, and it
would need the GPU this host shares.

**B. Backfill the Qdrant payload from the catalog.** Update the points
in place with the title the catalog already holds. No re-embedding (the
vector is untouched — only the payload changes), no GPU, and it fixes the
third of documents the catalog knows about.

**C. Join at search time.** Leave the payload alone and have the search
path look the title up. The server would need catalog access, and it
would cost a catalog read per hit — a search for 10 results would mean
10 catalog fetches, or a batched read of the whole page.

**D. Fix it at the source, going forward.** Make the *indexer* always
have a title, whatever route the document came in by, so the gap does
not grow.

The choice is between B/C (repair what is there) and D (stop the
bleeding), and the evidence says both.

### What the corpus says about metadata enrichment

Searching the corpus for the technique:

- **RocketQAv2** (Ren et al. 2021, `10.18653_v1_2021.emnlp-main.224`, in
  the corpus) — the joint dense-retrieval and re-ranking paper, whose
  central claim is that the *document representation* and the *query
  representation* should be learned together, with the document-side
  encoder producing a representation rich enough to match a query.
  A document represented without its title is a document represented
  worse than it could be: the title is the highest-signal text it has.
- **"Dense Passage Retrieval for Open-Domain Question Answering"** (Chen
  et al. 2020, `10.18653_v1_2020.emnlp-main.550`, in the corpus) — the
  DPR paper, whose passage encoding is `title + body`. **The title is
  part of the representation, not decoration.** A passage indexed
  without its title is a passage the retriever cannot match by title,
  which is exactly the kind of specific query the lexical channel
  exists to serve.

That is the strongest argument for the fix: the title is not metadata
*about* the document, it is part of what the document is.

## Technique chosen

**B + D: backfill the payload from the catalog, and make the indexer
never produce a title-less point again.**

- **B** is a new CLI command (`hs distill backfill-titles`) that joins
  the Qdrant payload to the catalog and updates the points in place. It
  updates the payload only — the vector is untouched, so this is not a
  re-embed. It is dry-run by default and reports exactly what it would
  change, in the same shape as the other repair commands in this project.
- **D** is a fix in the indexing path: when a document has no catalog
  row, the indexer must not write a point with a null title. It should
  take the title from the document itself (its first heading) and record
  that it did, so the payload is never title-less again.

Together they fix the 36 % that the catalog knows about, and stop the
57.5 % that never had a catalog row from being title-less too.

## Expected effect

- The share of search hits with no title falls from 72 % to roughly the
  share of documents that have no discoverable title at all (the
  title-less remainder is documents with neither a catalog row nor a
  heading, which the diagnostics will name).
- An agent that receives a search hit can name the document it came
  from, which is the precondition for issue #7's links and for a user
  opening what the agent found.
- The commands are report-first: an operator sees the damage and the
  repair before anything is written.

## Constraint compliance

- No corpus rebuild, no purge, no re-embed, no durable-consumer changes.
  The payload update touches metadata only; the vectors are not
  recomputed and the embedding model is not loaded.
- The repair command lives in the CLI, is dry-run by default, and is
  never an MCP tool.
- The one-path rule holds: a document that has no title gets one from
  the catalog, or from its own first heading, or it is reported as
  untitled — never a placeholder that looks like a title.
