# Run 2026-10-21-06 — the demoter that cannot see the corpus's bibliographies

## Problem, measured

The search path already has a reference-list demoter (rc.366). It is
wired in — `demote_reference_lists` is called on both paths out of
`DistillServerState::search` (`crates/hs-distill/src/api.rs:349` for the
lexical-empty path and `:608` at the end of `fuse_channels` for the
normal one). And it works on the bibliographies it recognises.

**The problem is that it does not recognise the bibliographies this corpus
actually contains.**

Measured on the live server, 2026-10-21, over the fixed 25-query set,
judged by an independent detector in the eval harness:

| | now |
|---|---|
| the **first** hit is a reference list | **7 / 25 queries = 28 %** |
| reference-like hits in the top 3 | 18/75 = 24 % |
| reference-like hits in the top 10 | 54/250 = 22 % |

The seven queries whose **first** hit was a bibliography, with the
document that came back and what the server's own detector scored that
chunk:

| query | the first hit | the detector's share |
|---|---|---|
| q03 `risk factors for falls in elderly people` | a 25-entry fall-prevention bibliography | 0.23 |
| q06 `nutrient limitation of tropical forest growth` | a 22-entry forest-ecology bibliography | 0.04 |
| q15 `oxytocin social behaviour and vasopressin` | a 12-entry neuroscience bibliography | 0.17 |
| q16 `cortisol stress response HPA axis` | a 21-entry psychiatry bibliography | 0.43 |
| q19 `knowledge discovery in data streams` | an all-caps APA bibliography | 0.04 |
| q20 `Hofstadter Gödel Escher Bach` | a numbered book bibliography | 0.00 |
| q25 `Rosalind Franklin diffraction data of DNA` | a numbered web-page bibliography | 0.22 |

The `share` column is `citation_line_share` — the share of the chunk's
lines that the server's `is_citation_line` calls a citation. Every one
of these chunks is a bibliography; the detector scores them 0.00–0.43,
all below its 0.5 threshold, so it calls them prose and they lead.

Two more bibliographies were found in the same set but did **not** lead
— q21 (`Waddington epigenetic landscape`, a 13-entry epigenetics
bibliography, share 0.08) and q22 (`3D printing pharmaceutical
bilayer tablets`, whose first hit is a **table**, not a bibliography, and
is correctly left alone). q21's bibliography is in the page but not
first, and q22's table is not noise at all.

**The expected passage is in the corpus for all of them.** Measured: for
q15, q16 and q21 the passage the query set expects is in the returned
page at rank 1–3, *behind* the bibliography. This is a **ranking**
failure, not a retrieval failure — nothing is missing from the corpus
and nothing needs re-indexing.

## The root cause

`crates/hs-distill/src/reference_list.rs` detects a citation line with a
**disjunctive** rule: any one of `doi:`, a parenthesised year after an
author list, `et al.`, a numbered `1.`/`2.` entry, `pp.`, `vol.`, a
`[n]` marker, a bare URL, a retrieval phrase, or a bulleted author list
makes the line a citation.

The demoter itself is correct and **is** wired in — `demote_reference_lists`
is called on both paths out of `DistillServerState::search`
(`crates/hs-distill/src/api.rs:349` for the lexical-empty path and `:608`
at the end of `fuse_channels` for the normal one). The defect is the
*rule set*: it covers the formats the corpus was sampled from when the
rule was written, and misses the formats this corpus mostly uses.

* **a bare year, not in parentheses** — `Sundralingham, P. 1983
  Responses of potted seedlings of Dryobalanops aromatica…`,
  `Ferguson JN, Aldag JM, Insel TR, Young LJ. Oxytocin in the medial
  amygdala is essential for social recognition. J Neurosci 2009;29:1487-93.`
  The rule looks for `(2020)`; these styles write `2020.`
* **an ALL-CAPS author list** (APA) — `ALBERG, D., LAST, M., AND KANDEL,
  A. 2012. Knowledge discovery in data streams…`
* **a full given name** — `Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua
  Bengio. 2015. Neural machine translation…`
* **a reference number other than 1 or 2** — `59. Ferguson JN, …`,
  `180 Daban C, …`, `(34) Lewis, J. A. …`. `is_numbered_entry` only
  strips `1.` and `2.`
* **a semicolon-separated author list** — `Usunier, N.; Synnaeve, G.;
  Lin, Z.; and Chintala, S. 2016. Episodic exploration…`
* **a book bibliography** — `Hofstadter, Douglas R. Gödel, Escher, Bach:
  An Eternal Golden Braid (New York: Basic Books, 1979), 312.`

The corpus's *actual* bibliography formats, read off the failing chunks:
a 25-entry list where every line is `- 5.Tinetti ME, Speechley M,
Ginter SF. Risk factors for falls among elderly persons living in the
community. N Engl J Med 1988;318:857-62.` (q03); a 22-entry list of
`Surname, I. 1983 Title. Journal.` (q06); a 17-entry list of
`(32) Hull, C. W. Apparatus for production of three-dimensional objects…`
(q22). None of those three shapes is in the rule's set.

## The technique, and what the literature says

Searched the corpus first (`hs distill search`), because home-still
should be used on itself.

**The corpus's own prior art.** `10.1007_s10648-025-10073-9` describes
its own preprocessing: *"we then removed the abstract and references
from the output using regular expressions"* — rule-based reference
removal is the established, published practice, not a heuristic invented
here.

**The literature.** *A Hybrid Approach and Unified Framework for
Bibliographic Reference Extraction* (arXiv 1912.07266) states the
standard heuristic and its limit directly: *"To detect such references,
adopted heuristics will look for comma-separated author names at the
start of the reference string. The drawback of such an approach is that
it will not be able to detect a reference if it does not comply with
defined heuristics… Every domain has its unique referencing style and
sometimes multiple referencing styles exist within one domain, making
simple approaches unsuitable for this complex task."*

Two things follow, and they are the design:

1. **the signal is the author list, not the punctuation** — a
   comma-separated author list at the start of the line is the shape
   every style shares; and
2. **multiple styles coexist, so a single disjunctive rule cannot cover
   them — a rule set is needed, and each rule must be narrow.**

The same paper names the tool that does this at scale: **Citation-Parser**,
*"a typical example of heuristics based tool that employs carefully
designed heuristics to identify components of bibliographic reference
strings (authors, title, conference/journal, etc.)"*.

**Evaluation.** The literature's own metrics (citation precision and
recall — arXiv 2304.09848) are the right frame: a reference-line
detector is a precision/recall problem, and a rule added without a
precision check is a regression waiting to happen. `Document
Navigability` (arXiv 2206.10253) adds the complementary observation that
reference *sections* are found by a keyword heuristic on the heading
(`References`, `Bibliography`, `Literature Cited`) — which is what this
corpus's q25 chunk shows in the raw text.

## The technique chosen

**Keep the detector disjunctive, and add one narrow, conjunctive rule
for the shape the corpus actually uses: an author list plus a bare
year.**

The new rule fires when a line has **both**:

* a **standalone year** — a four-digit year (1950–2029) that is its own
  token, bounded by whitespace or punctuation, so `2000>VPD` and
  `2013;44(4)756` do not match; **and**
* an **author-list shape** — one of
  * `Surname I` / `Surname IJ` — a capitalised surname followed by one or
    two capitalised initials (`Ferguson JN`, `Lewis, J. A.`);
  * `Surname, I.` — a capitalised surname, a comma, a single initial;
  * `Surname, G.` with an ALL-CAPS surname — the APA style
    (`ALBERG, D.`);
  * `Given Surname, Given Surname,` — the full-name style
    (`Dzmitry Bahdanau, Kyunghyun Cho,`).

The conjunction is the whole point. The server's existing rules are
deliberately conservative *because* they are disjunctive — one loose
pattern and prose is caught. A **conjunctive** rule needs both signals
to fire, so it can afford a looser notion of "author list" without
catching prose: a prose sentence has a year or a number, but it does not
have a capitalised surname followed by initials.

A second narrow rule covers the numbered style the corpus uses: a
reference number (`59.`, `180 `, `(34)`) followed by an **author-shaped**
token. The number alone is not enough — `22. The idea of the series is,
in itself, important to Eisenman's early development` is a numbered
footnote, not a bibliography — so the token after the number has to name
an author.

**The threshold stays at 0.5**, and the demotion stays a demotion: the
chunk is not dropped, it is moved below prose, and it keeps its score.

## Expected effect

* the six queries that open with a bibliography open with **prose**;
* the expected passage moves from rank 1–3 to rank 1 for q15, q16, q21;
* **RECALL@10 must not drop** — the demotion reorders, it does not add
  or drop, but a bibliography that *is* the answer (q25) must still be
  reachable;
* the false-positive rate on prose must be **0** on the measured pool —
  the precision half of precision/recall, and the reason the rules are
  conjunctive.

## What actually happened

Measured A/B on the live corpus, 2026-10-21: the running rc.373 server
on `:7434` against the new rc.374 server on `:7436`, both reading the
same Qdrant collection, both on CUDA, the same 25-query set, judged by
an **independent** detector in the eval harness.

| | rc.373 | rc.374 |
|---|---|---|
| the first hit is a reference list | 7/25 = 28 % | **1/25 = 4 %** |
| reference-like hits in the top 3 | 18/75 = 24 % | **3/75 = 4 %** |
| reference-list share of the page | 54/250 = 22 % | **30/250 = 12 %** |
| RECALL@10 | 24/25 = 96 % | **25/25 = 100 %** |
| median latency | 149 ms | 150 ms |

**Recall went up**, not just "not down". The mechanism is the same as
the fix: for `q25` the expected passage sat at rank 11, pushed out of
the top 10 by the bibliography that outranked it; demoting the
bibliography promoted the passage back in, to rank 9.

The one query that still opens with a bibliography is `q14`, and it is a
**mixed** chunk — a citation list with a line of prose attached, at a
share just under the 0.5 threshold. A chunk that mixes a citation with
real prose discusses the topic and cites it, and demoting it would hide
the prose. That is the right trade, and the threshold is unchanged.

**Precision.** The new rules can only *add* detections, so the question
is how many of the chunks they newly call bibliographies are not. On a
761-chunk pool (every distinct chunk the query set returns at a limit
of 50 per query) the new rules call **91 (12.0 %)** a reference list,
and **all 91 were read by eye: every one is a genuine bibliography**.
Zero false positives.

**Three bugs the tests caught**, each of which would have shipped a
detector that quietly did nothing:

* `has_author_list_shape` split the line on commas, so `Sundralingham,
  P.` — a surname and an initial separated by a comma — never matched.
  The scan is over **author pairs**, not whitespace-separated tokens.
* `has_standalone_bare_year` claimed `Genes Dev. 2009, 23, 939-950`,
  which is a journal, a volume and a page range, not a bare year.
* the initials check accepted only *one* initial, so `Tinetti ME` — two
  — did not match.

## What this is not

* Not a re-index. No vector is recomputed, no chunk is re-cut: the
  demotion happens at query time, over the chunks as they are.
* Not a corpus change. Nothing is rebuilt, purged or re-embedded.
* Not a cross-encoder. The backlog's first item is cross-encoder
  reranking, and it remains the right long-term answer — but it is
  blocked on a second GPU service and on the 3090 being free, and this
  fix is the 80 % that does not need either.
