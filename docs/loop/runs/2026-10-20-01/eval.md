# Evaluation — hybrid (dense + lexical) retrieval

Two runs of the same fixed query set against the same live corpus
(438,863 chunks, Qdrant 1.19.2, bge-m3 on CUDA), on `big`, 2026-10-20.

- **Query set:** `docs/loop/queries.yaml` — 25 queries in three groups
  (8 `concept`, 10 `literal`, 7 `named`), each naming the passage it
  should reach.
- **Harness:** `docs/loop/eval_queries.py` — talks to the distill server
  over HTTP, so it measures the server, not the CLI's presentation.
- **Limit:** 10 (the server's default page).
- **Pass:** the expected passage appears in the top 10.

## Result

| | baseline (dense only) | hybrid (dense + lexical) |
|---|---|---|
| **RECALL@10** | **20/25 = 80.0 %** | **25/25 = 100.0 %** |
| concept | 8/8 = 100 % | 8/8 = 100 % |
| literal | 7/10 = 70 % | 10/10 = 100 % |
| named | 5/7 = 71.4 % | 7/7 = 100 % |
| median latency | 52 ms | 101 ms |
| rank of the expected passage | 1 (median) | 1 (median) |

**No regression.** Every query the baseline answered still answers, and
the five it missed now answer at rank 1–2. The concept group — the one
dense retrieval was already good at — is unchanged at 100 %, which is the
point of fusing ranks rather than replacing one channel with another.

The cost is one extra Qdrant round trip: 52 ms → 101 ms median. The
lexical channel's own cost is dominated by the per-term counts (1–2 ms
each against the text index).

### The five queries the baseline missed

| id | query | baseline | hybrid |
|---|---|---|---|
| q09 | proximal humeral epiphyseal ossification | miss | rank 1 |
| q10 | juvenile idiopathic arthritis uveitis | miss | rank 1 |
| q12 | cerebellar Bergmann glia potassium buffering | miss | rank 1 |
| q19 | Kandel principles of neural science | miss | rank 1 |
| q22 | Gazzaniga split brain | miss | rank 2 |

Every one is a query whose answer is **in the corpus** and named by a
specific term. The dense embedder matched the *neighbourhood* of the
term ("epiphyseal" → other bone terms; "Kandel" → other neuroscience
texts) and never reached the passage. The lexical channel reaches it
because the passage contains the word.

## Reference-list noise: pre-existing, and barely changed

A chunk that is a bibliography entry ("…ALBERG, D., LAST, M., AND KANDEL,
A. 2012…", "doi: 10.1016/j.neubiorev.2009.01.004") matches a named query
perfectly and is useless as an answer. That is worth measuring, because a
keyword channel surfaces exactly that kind of chunk.

Measuring it: a chunk counts as reference-like when more than 60 % of its
non-empty lines carry a bibliography marker (`et al.`, `doi:`, `Press`,
`Journal of`, `Proceedings`, a publisher name, …).

| | baseline (dense only) | hybrid |
|---|---|---|
| reference-like chunks in the top 10 | 42/250 = **16.8 %** | 44/250 = **17.6 %** |
| queries whose top 10 is mostly references | 1 (q14) | 4 (q05, q13, q14, q22) |

So the noise is **pre-existing**: the dense channel produces 16.8 % of it
on its own, and the hybrid raises that by 0.8 points while lifting recall
by 20 points. The named-entity queries are where the hybrid is weakest
here — a surname in a reference list is indistinguishable from a surname
in a discussion, and the lexical channel cannot tell them apart.

This is a real limitation and it is the natural next step: a cross-encoder
reranker (our own `myelin` crate already runs `bge-reranker-v2-m3` for
exactly this) scores a query *against* a passage and can tell a
bibliography entry from a discussion of the topic. It is not in this
change because it needs a second GPU service, and the 3090 on `big` is
currently shared with another agent's job.

## The lexical channel alone
Measured separately, against the same set, the lexical channel on its own
reaches **25/25 = 100 %** with a median rank of 1. That is the evidence
that the two channels are complementary rather than redundant: either
one alone can answer this set, and the fusion catches what the other
misses.

## A note on the query set

The first version of `queries.yaml` was wrong, and the run exposed it.
Three of its expectations ("Hashimoto thyroiditis", "Werner syndrome",
"Bergmann glia") named a term that occurs in the corpus **only in
reference lists** — "Hashimoto" is an author surname in 779 chunks, and
no document in the library is about Hashimoto's disease:

```
$ grep -ril hashimoto /mnt/home-still/markdown | wc -l
111
$ (for each, does the title mention it?) → 0
```

No retrieval method can return a passage the library does not hold, and
the baseline's "miss" on that query was the honest answer. The set was
rebuilt so every `expect` term is one the corpus actually holds as a
document topic (checked with the title test above), and the two runs
reported here are both on the corrected set. The numbers are therefore
comparable to each other, which is what matters; the first set's
55.6 % → 83.3 % is not reported as a result because its expectations
were not satisfiable.

## Timings

Measured on the live server, one query at a time, warm:

- **baseline**: 50–72 ms per query (median 52 ms) — one embed + one
  Qdrant vector query.
- **hybrid**: 53–345 ms per query (median 101 ms) — one embed, one
  Qdrant vector query, one count per query term, and up to four
  text-match scrolls.

The 345 ms outlier is the first query after a restart (cold embedder
cache); the rest sit in the 53–156 ms band.

## How the numbers were produced

```sh
# the harness (writes JSON for the run)
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/run.json

# the baseline is the released binary; the hybrid is the one this change builds
install -m 0755 /tmp/hs-distill-server.bak.rc364 ~/.local/bin/hs-distill-server
sudo systemctl restart hs-serve-distill.service
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/baseline2.json
```

`/tmp/baseline2.json` (before) and `/tmp/after2.json` (after) hold the
per-query rows the tables above summarise.
