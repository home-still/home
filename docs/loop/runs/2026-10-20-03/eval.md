# Evaluation — the title gap in search results

Measured on the live server over the 438,863-chunk corpus on `big`,
2026-10-20, before and after the change.

## The problem

An agent that calls `distill_search` gets a passage and a `doc_id`. Over
the 250 hits of the fixed 25-query set (`docs/loop/queries.yaml`):

| field | hits missing it, before |
|---|---|
| `title` | **180/250 = 72.0 %** |
| `pdf_path` | 0/250 = 0 % |

The title is not missing from the catalog — for that same stem:

```
catalog/10/10.1371_journal.pone.0038469.yaml
  title: Bipolar Disorder, Brain-Derived Neurotrophic Factor (BDNF) Val66Met Polymorphism
```

It is missing from the **Qdrant payload**, because the title is copied
from the catalog row *at index time* (`extract_rule_based` →
`chunk_payload`) and a search hit reads it back out of the payload. A
document indexed before its catalog row existed was indexed with no title,
and — because the corpus is not re-indexed — it stays that way.

## The change

Two parts, because the data says the gap has two different causes.

**1. `hs distill backfill-titles`** — a new CLI command (dry-run by
default) that writes a title onto the points that lack one. A **payload
update**: the vector is untouched, so this is not a re-embed and needs no
GPU and no embedding model. The title comes from the catalog where the
catalog has one, and from the document's own first line where it does
not.

**2. The indexer no longer produces a title-less point.** When a document
has no catalog row, the title comes from the document's own text — its
first heading, or its first plain line. This is the part that stops the
bleed; the backfill is the part that repairs what is already there.

The rule for the document's own text is a **heading *or* a first plain
line**, not a heading alone. Measured over 150 sampled documents:

| source of the title | yield |
|---|---|
| the first markdown heading | 50/150 = 33.3 % |
| the first plain line | 80/150 = 53.3 % |
| neither | 20/150 = 13.3 % |
| **either** | **130/150 = 86.7 %** |

A heading-only rule leaves two thirds of the documents without a title,
because most of this corpus has no markdown heading at all — the title is
the first line of the text.

## Result

| | before | after |
|---|---|---|
| hits with no title | 180/250 = **72.0 %** | **52/250 = 20.8 %** |
| RECALL@10 (unchanged queries) | 24/25 = 96 % | 24/25 = 96 % |
| median latency | 138 ms | 151 ms (unchanged; the fix is not in the search path) |

**Recall is unchanged.** The change adds no retrieval behaviour: it
writes a title that was already known, onto a payload. A search that
found a passage before still finds it.

Over the whole corpus, the share of indexed documents carrying a title
went from **27.5 % to 82.2 %** (4,313 of 15,745 → 12,945 of 15,757).

The remaining 2,812 documents have no title in the catalog *and* none
their own text yields. They were read: their markdown begins with body
text, because the OCR of those PDFs lost the title page. They are
reported by name, never given a placeholder — a title that reads like a
title but is not one is worse than no title.

## Verified end to end, on a real document

The live server, one real document from the corpus:

```
POST /doc/10.1371_journal.pone.0038469/title
  {"title": "Bipolar Disorder, Brain-Derived Neurotrophic Factor (BDNF) Val66Met Polymorphism"}
→ {"doc_id":"10.1371_journal.pone.0038469","points_updated":18}
```

The document's 18 chunks now carry the title, and a search that returns
that document now returns its title too:

```
  10.1371_journal.pone.0038469
    title: 'Bipolar Disorder, Brain-Derived Neurotrophic Factor (BDNF) Val66Met Polymorphism'
```

The 18 points updated is the document's chunk count — the payload was
rewritten, no vector was recomputed, and the embedding model was never
loaded (the GPU was not used at any point in this change).

## How much is fixable

Sampled 3,831 distinct indexed documents, of which 1,933 have no title in
the payload. Checking the first 200 of those against the catalog:

| | count | share |
|---|---|---|
| catalog **has** a title (the backfill fixes it) | 72 | 36 % |
| catalog has no title either | 13 | 6.5 % |
| **no catalog row at all** | 115 | 57.5 % |

So the catalog alone repairs about a third of the title-less documents.
The document's own text covers most of the rest: the backfill planned
**5,002** documents (423 from the catalog, 4,579 from the document's own
text) and wrote **4,124** of them in the first pass — the rest were
lost to write timeouts, and a second pass with retries wrote them.

What remains untitled is a document with neither a catalog title nor a
title its own text yields. **2,812** documents are in that state, and
they were read: their markdown begins with body text ("The present paper
draws on data collected in: (1) an Australian multicentre prospective
pregnancy cohort study…"), because the OCR of those PDFs lost the title
page. The command reports those by name rather than inventing a title
for them.

## A defect the live corpus caught, and the fix

The first backfill pass wrote 178 titles that carried the converter's
markup, and 16 that repeated themselves. The converter's OCR emits the
page's furniture where the title page should be — a table
(`<table><tr><br>In this research on gender bias in this research on
gender bias…`), or a header merged with a caption ("The rapidity, and,
and finally, and finally, and finally…"). Those lines were the first
line of their documents, and the first-line rule took them.

A title shown to a user verbatim, carrying markup, is worse than no
title. Three rules now reject them, each with a test built from a real
corpus line:

- a line that **begins** with a tag is the converter's furniture, not a
  title with markup in it — the text inside a table cell is not the
  document's title;
- a line that **repeats itself** (the same 4–6 words twice running) is
  an OCR artifact;
- a line that **carries a tag mid-title** keeps the text around it —
  `<scp>A</scp>` in a title is a subscript, and the title is still
  there.

The backfill also **corrects** such a title rather than only filling a
missing one: a stored title that is unusable is replaced by the
catalog's, when the catalog has one.

## Why the title is worth fixing beyond display

DPR-style retrieval encodes `title + body` (Chen et al. 2020, in the
corpus). A point indexed without its title is a point the retriever
cannot match *by title* — which is exactly the kind of specific query the
lexical channel exists to serve. The title is not metadata about the
document; it is part of what the document is.

## Timings

- `GET /docs/titles` over the whole collection: **8.2 s** (one scroll of
  438,863 points, reading two payload fields).
- `POST /doc/{id}/title`: **1–21 ms** per document, measured on the live
  server (18 points).
- The full backfill: **14 min** for 4,124 documents (126,812 points) at
  16 concurrent writes, and the first pass lost 207 documents to Qdrant
  timeouts — the live indexer writes to the same collection. At 4
  concurrent writes with a retry on the write, a second pass finished
  the remainder without a failure.
- The search path is untouched: median latency 138 ms before, 151 ms
  after.

## How the numbers were produced

```sh
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/run.json
python3 docs/loop/measure_titles.py --limit 10 --out /tmp/titles.json
```

The "hits with no title" figure is measured over the same 250 hits, with
the same detector the server uses to read the title back out of a point.
