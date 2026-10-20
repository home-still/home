# Evaluation — reference-list demotion in search results

Two runs against the same live server, the same corpus (438,863 chunks,
Qdrant 1.19.2, bge-m3 on CUDA) and the same fixed query set
(`docs/loop/queries.yaml`, 25 queries), on `big`, 2026-10-20.

- **Before:** `v0.0.1-rc.365` (the released version — hybrid dense +
  lexical, RRF-fused).
- **After:** the candidate change (same, plus the reference-list
  demotion).
- **Harness:** `docs/loop/eval_queries.py` — talks to the distill server
  over HTTP, so it measures the server, not the CLI's presentation.
- **Limit:** 10 (the server's default page).
- **Reference-like:** a chunk whose share of citation lines exceeds 0.5,
  measured with the same detector the server now uses
  (`crates/hs-distill/src/reference_list.rs`).

## Result

| | before (rc.365) | after |
|---|---|---|
| **RECALL@10** | 24/25 = 96 % | **24/25 = 96 %** |
| concept | 8/8 = 100 % | 8/8 = 100 % |
| literal | 10/10 = 100 % | 10/10 = 100 % |
| named | 6/7 = 85.7 % | 6/7 = 85.7 % |
| **reference-like in the top 3** | 26/75 = **34.7 %** | 7/75 = **9.3 %** |
| **reference-like in the top 10** | 81/250 = **32.4 %** | 18/250 = **7.2 %** |
| median latency | 121 ms | 130 ms |

**Recall did not change** — that was the constraint. A third of the
answers a user was shown were bibliographies; now it is a tenth in the
top 3 and a fifteenth in the top 10.

## What the user sees

`hs distill search "Gazzaniga split brain"`, before:

```
1.  [no title]  ice and rats. Neuropharmacology 62, 1191–1203. doi: 10.1016/…
2.  [no title]  anges in patients with a first episode of major depression…
3.  The magical number 4 in short-term memory
```

Both of the first two are reference lists. The user asked a question and
got a bibliography.

After:

```
1.  10.1038_s41598-017-00421-3   (a passage about hemispheric connectivity)
2.  Three-Dimensional Printing of Wood-Derived Biopolymers
3.  The myth of language universals
```

The reference lists are still in the results — demoted, not dropped.

## The rule, and the two bugs that shaped it

The demotion is a **stable partition**: the prose keeps its fused order,
the reference lists keep theirs, and the prose is placed first. Two
properties, both measured:

- **The page keeps its hits.** The demotion reorders; it never adds or
  drops, so the caller's `limit` still decides how many results there
  are.
- **A reference list leads only when there is no prose at all.** When the
  page holds prose, the prose leads — even if a reference list outranks
  it. For "Gazzaniga split brain" the best-ranked hit was a chunk of
  `doi:` lines scoring 0.500 while the passage that discusses the topic
  scored 0.250, and the user asked a question, not for a bibliography.
  When the page holds *no* prose, the reference list is the answer and
  keeps its place: sometimes the only chunk in the corpus that mentions
  a term is a citation of it.

Two earlier versions of this rule were wrong, and the live server caught
both:

1. **"Demote every reference list past every prose chunk."** A sort. It
   lost the expected passage for two of the 25 queries — recall 25/25 →
   23/25 — because for those queries the expected passage was itself a
   reference list. A user asking "what does this corpus say about
   Dawson and Krebs?" wants that chunk whether or not it is formatted as
   a bibliography.
2. **"Demote only past prose that outranks the reference list."** Too
   weak: for most queries the reference list *does* outrank the prose
   (the dense embedder matches a citation perfectly well), so the
   demotion did nothing — measured, the reference share in the top 3
   went 18/75 → 28/75, i.e. *up*.

The rule that works is the "prose exists → prose leads" one, and it is
the third.

## A detection bug this iteration found and fixed

The first implementation of the detector missed two real formats in this
corpus, and the live server showed it immediately — the CLI still showed a
reference list first after the first build:

1. **Markdown bullets.** Some bibliographies are rendered as a bulleted
   list (`- Frodl T, Meisenzahl EM, Zetzsche T, et al.  Hippocampal and
   amygdala …`) with **no year at all**. There was nothing to match on
   but the shape of the author list.
2. **A long author list.** In `Butters, M. A., Becker, J. T., Nebes, R.
   D., …, et al. (2008) …` the year is **103 characters** in. The first
   implementation capped the distance between the first surname and the
   year at 60 characters, so it missed the line.

Both are now covered by tests built from real corpus lines
(`a_bulleted_author_list_is_a_citation`,
`a_long_author_list_is_a_citation_even_with_the_year_far_away`), and
the detector is 17 unit tests in total.

The lesson the live check taught: a detector validated only on synthetic
examples would have shipped broken. Every pattern in the detector now
has a test taken from a line the corpus actually contains.

## A wrong expectation in the query set

The first version of this iteration's query set expected
"Dawson and Krebs tricarboxylic acid cycle" to find a passage about the
Krebs cycle. The corpus does not hold one: `grep` finds 137 chunks
mentioning "Dawson", and every one of them is the botanist T. E. Dawson
in a reference list. The only chunk that mentioned both names was a
bibliography entry citing Dawson's paper.

So the expectation was wrong, not the search. The query was replaced with
one the corpus actually holds ("Watson and Crick structure of DNA", whose
expectation *is* reachable), and the same check was done for q25
("Lorenz" — also only ever a citation in this corpus). The numbers in
the table above are both from the corrected set, so they are comparable
to each other. The earlier figures on the uncorrected set are not
reported as results.

## How the numbers were produced

```sh
# the harness
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/run.json
```

`/tmp/base-rc365.json` (before) and `/tmp/final-rc366.json` (after)
hold the per-query rows the tables above summarise. The reference-share
figures are measured with the same detector the server uses, applied to
the same results.
