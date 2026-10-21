# Evaluation — the demoter that cannot see the corpus's bibliographies

Measured on the live corpus on `big`, 2026-10-21.

## The problem

The search path already demoted reference lists below the passages
that discuss the topic (rc.366). The demoter was wired in and correct
— `demote_reference_lists` is called on both paths out of
`DistillServerState::search` (`crates/hs-distill/src/api.rs:349` for
the lexical-empty path, `:608` at the end of `fuse_channels` for the
normal one).

Its **detector** could not see the bibliographies this corpus actually
contains. A bibliography entry matches a query about a topic perfectly
well in a dense embedding — semantically, a citation to work about
oxytocin *is* about oxytocin — so the bibliography wins the ranking and
the user gets a list of citations instead of a passage.

Measured on the live server, over the fixed 25-query set, judged by an
**independent** detector in the eval harness:

| | before |
|---|---|
| the **first** hit is a reference list | **7/25 = 28 %** |
| reference-like hits in the top 3 | 18/75 = 24 % |
| reference-like hits in the top 10 | 54/250 = 22 % |

The seven, with what the server's own detector scored the chunk:

| query | the first hit | the detector's share |
|---|---|---|
| q03 `risk factors for falls in elderly people` | a 25-entry fall-prevention bibliography | 0.23 |
| q06 `nutrient limitation of tropical forest growth` | a 22-entry forest-ecology bibliography | 0.04 |
| q15 `oxytocin social behaviour and vasopressin` | a 12-entry neuroscience bibliography | 0.17 |
| q16 `cortisol stress response HPA axis` | a 21-entry psychiatry bibliography | 0.43 |
| q19 `knowledge discovery in data streams` | an all-caps APA bibliography | 0.04 |
| q20 `Hofstadter Gödel Escher Bach` | a numbered book bibliography | 0.00 |
| q25 `Rosalind Franklin diffraction data of DNA` | a numbered web-page bibliography | 0.22 |

The share is `citation_line_share` — the share of the chunk's lines
the server's `is_citation_line` calls a citation. Every one of these
chunks is a bibliography, and every one scores **below the detector's
own 0.5 threshold**, so it calls them prose and they lead.

**The expected passage was in the corpus for all of them.** For q15,
q16 and q21 the passage the query set expects is in the returned page
at rank 1–3, *behind* the bibliography. This is a **ranking** failure,
not a retrieval failure: nothing was missing from the corpus, and
nothing needed re-indexing.

## The change

`is_citation_line` is **disjunctive** — a `doi:`, an `et al.`, a
parenthesised year, a `pp.`, a `vol.`, a `[n]`, a bare URL, a
retrieval phrase, or a bulleted author list makes a line a citation —
and that rule set was written against a sample of formats this corpus
does not use. It looks for a year **in parentheses** (the corpus
writes a **bare** one) and `is_numbered_entry` strips only `1.` and
`2.` (the corpus numbers its references into the hundreds).

Three new rules, each **conjunctive**, so that a loose notion of
"author list" cannot catch prose:

* an **author list AND a bare year** — `Sundralingham, P. 1983 …`,
  `ALBERG, D., LAST, M., AND KANDEL, A. 2012. …`,
  `Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua Bengio. 2015. …`;
* a **reference number AND an author** — `59. Ferguson JN, …`,
  `(34) Lewis, J. A. …`;
* a **bullet holding a numbered entry** — `- 5.Tinetti ME, …`.

The threshold stays at 0.5 and the demotion stays a demotion: the
chunk is not dropped, it is moved below prose, and it keeps its score.

## Result

Measured **A/B on the same corpus**: the running rc.373 server on
`:7434` against the new rc.374 server on `:7436`, both reading the
same Qdrant collection, both on CUDA, the same 25-query set.

| | rc.373 (before) | rc.374 (after) |
|---|---|---|
| the **first** hit is a reference list | 7/25 = 28 % | **1/25 = 4 %** |
| reference-like hits in the top 3 | 18/75 = 24 % | **3/75 = 4 %** |
| reference-like hits in the top 10 | 54/250 = 22 % | **30/250 = 12 %** |
| RECALL@10 | 24/25 = 96 % | **25/25 = 100 %** |
| median latency | 149 ms | 150 ms |

**Recall went up, not down.** The constraint was "must not drop"; it
improved, and the mechanism is the same as the fix. For `q25` the
expected passage sat at **rank 11** of the page — pushed out of the
top 10 by the bibliography that outranked it. Demoting the
bibliography promoted the passage back in, to rank 9. The six queries
whose first hit changed all changed the same way: from a bibliography
to prose.

**The one query that still opens with a bibliography** is `q14`
("hippocampal neurogenesis and adult memory"), and it is a **mixed
chunk** — a citation list with a line of prose attached, at a share
just under 0.5. A chunk that mixes a citation with real prose
discusses the topic and cites it, and demoting it would hide the
prose. The threshold is deliberately conservative, and this is the
right trade.

## Precision — the false-positive rate, measured

The new rules can only *add* detections, so the question is: of the
chunks they newly call reference lists, how many are not
bibliographies?

Measured on a **761-chunk** pool (every distinct chunk the fixed query
set returns at a limit of 50 per query): the new rules call **91
(12.0 %)** a reference list, and **all 91 were read by eye — every one
is a genuine bibliography**. **Zero false positives.**

The one that looked like prose at first — a paper's conclusion
followed by its `References` section — is a chunk that is *mostly*
bibliography, and the 0.5 threshold is what decides that correctly.

## The three bugs the tests caught

The first implementation of the new rules was wrong three times, and
the tests found each. A detector validated only on synthetic examples
would have shipped all three:

* `has_author_list_shape` split the line on commas, so `Sundralingham,
  P.` — a surname and an initial separated by a comma — never matched.
  The scan is over **author pairs**, not whitespace-separated tokens.
* `has_standalone_bare_year` claimed `Genes Dev. 2009, 23, 939-950`,
  which is a journal, a volume and a page range, not a bare year.
* the initials check accepted only *one* initial, so `Tinetti ME` — two
  — did not match.

## Verified end to end, on the live corpus

The same three queries through both servers:

| query | rc.373's first hit | rc.374's first hit |
|---|---|---|
| "oxytocin social behaviour and vasopressin" | a 12-entry bibliography of neuroscience citations (1,890 ms) | *Human endogenous oxytocin and its neural correlates show adaptive responses to social touch based on recent social context…* (1,263 ms) |
| "nutrient limitation of tropical forest growth" | a bibliography (285 ms) | the paper that discusses it (199 ms) |
| "3D printing of controlled release pharmaceutical bilayer tablets" | a table chunk (352 ms) | the same table chunk (300 ms) — **a table is not a bibliography, and it is not demoted** |

The third row is the false-positive check in the wild: q22's first hit
is an HTML table that OCR read as text, and the new rules leave it
alone.

## Timings

- The search path is **untouched**: median latency 149 ms before,
  150 ms after, on the same corpus and the same query set. The
  demotion is a partition of a list that is already in hand — it costs
  one pass over the page.
- No vector is recomputed and no chunk is re-cut. The demotion
  happens at query time, over the chunks as they are.

## How the numbers were produced

```sh
# the before: the running release on :7434
python3 docs/loop/eval_queries.py --limit 10 --server http://127.0.0.1:7434 --out /tmp/before.json
# the after: the new build on :7436, same Qdrant, same CUDA
python3 docs/loop/eval_queries.py --limit 10 --server http://127.0.0.1:7436 --out /tmp/after.json
```

The eval harness now carries its own `reference_like()` detector, so
the metric is **independent of the code under test** and is reported
by the project's own tooling on every run:

```
RECALL@10: 24/25 = 96.0%
  the first hit is a reference list: 7/25 = 28.0%
  the reference-list share of the page: 54/250 = 21.6%
```

The two servers were run **side by side** against the live corpus: the
new `hs-distill-server` was built with `--features server,cuda` (the
project refuses a CPU embedder) and run on an alternate port with
`INVOCATION_ID` set, which is the documented way past the
`hs serve distill` systemd-conflict guard. The live service on `:7434`
was never touched.
