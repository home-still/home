# Research — every glm_ocr conversion is being thrown away

**Date:** 2026-10-23
**Question:** why did the last 24 hours of conversions add almost nothing
to the corpus, and what is the smallest change that stops it?

---

## 1. The symptom, measured

| measurement (live, 2026-10-23) | value |
|---|---|
| markdown objects, change over ~10 h | 15,877 → 15,884 (**+7**) |
| documents received by the two scribe watchers in 24 h | 176 |
| … ended `terminating (will not redeliver)` | **176 / 176** |
| … `gapped_conversion` (bmb glm_ocr) | **154** (118 + 36) |
| … `empty_conversion` / `paywall_html` | 9 / 4 |
| olmocr tier runs with `Completed pages: 0` | 11 / 11 (P0-27, separate) |

`gapped_conversion` means the server returned markdown with regions it
"could not process", and since 2026-10-09 (`f0f5623`) such a conversion is
refused and the event is never redelivered. The document is lost to the
corpus until someone re-ingests it by hand.

## 2. The cause, attributed (not assumed)

bmb's scribe log, last 24 h: the "could not process" regions are, almost
all, **"streaming repetition detector aborted region"** — reasons
`N-gram-cycle` 2,655, `trigram-cycle` 556, `bigram-cycle` 38, and table-cell
loops 64 + 13 + 8. None are `region_type=Table` (tables reach the VLM one
cell at a time).

The detector (`crates/hs-scribe/src/ocr/repetition_detector.rs`) counted
n-grams in a 256-word window and fired at 10 of one bigram, 5 of one
trigram, or **3 of one 4–6-gram**. Its own header conceded false positives
on bibliographies; the calibration document it cited is not in the repo.

To measure the false-positive rate, the detector was replayed, word by
word with a check after each word (as the stream does), over every block
of 200 seeded **accepted** documents (seed 20261023; 46,083 blocks):

| converter | documents with ≥ 1 aborted block |
|---|---|
| olmocr (never passed through the detector) | **80 / 83 (96.4 %)** |
| scribe-vlm | 65 / 72 (90.3 %) |
| glm_ocr (already survived it) | 31 / 43 (72.1 %) |

The aborted blocks are ordinary academic text: an affiliation list naming
"University of California–San Diego, La Jolla, California" three times; a
citation run with "et al., 20xx;" ten times; a results paragraph that says
"the number of choices" four times. A 4-gram three times in 256 words is
how people write. Because one aborted region rejects the whole document,
the detector was dropping nearly every document it saw.

## 3. What the literature says a loop is

- **Xu et al. 2022, *Learning to Break the Loop*** (arXiv 2206.02369, in
  the corpus): models with maximization decoding "get stuck in redundant
  **consecutive** repetitions", while "there are few consecutive
  sentence-level repetitions in human corpora (e.g., **0.02 %** in
  Wikitext-103)". And repetition is **self-reinforcing**: "the probability
  of repetition increases almost monotonically with the number of
  historical repetitions". A loop, once begun, does not end.
- **Blecher et al. 2023, *Nougat*** (arXiv 2308.13418, in the corpus),
  academic-PDF OCR with a transformer decoder: "In its simplest form, the
  last sentence or paragraph is repeated over and over again" — 1.5 % of
  test pages. It also notes near-copies (a few words changed each time)
  defeat any strict detector.
- **Holtzman et al. 2020, *The Curious Case of Neural Text
  Degeneration*** (in the corpus): beam search "gets stuck in a repetition
  loop it cannot escape".

So the discriminating signal is **adjacency**: the same unit back to back.
Recurrence of a phrase *somewhere* in a window is normal prose; the same
unit three times *consecutively* is ~never human text.

## 4. The change

Fire only when the tail of the stream is **≥ 3 consecutive copies of one
unit of `p` words (1 ≤ p ≤ 128), spanning ≥ 32 words**. Kept per period:
`run[p]`, the number of consecutive newest words equal to the word `p`
before them; the tail is `p`-periodic over `run[p] + p` words. O(128) per
word, fixed-size state, no allocation (the word is hashed while it is
normalized). `LoopReason` keeps its wire form (period 2 → `bigram`, 3 →
`trigram`, else `n_gram(p)`).

The asymmetry justifies waiting: a real loop never stops, so firing a few
dozen words later costs only those tokens; a false positive costs the
document.

Not caught: paraphrase loops and units over 128 words. They run to the
token limit and `clean_repetitions_per_page` handles them, as it did
before the streaming detector existed.

## 5. Expected effect, measured offline (same 200 documents)

| | old rule | new rule |
|---|---|---|
| olmocr documents with an abort on accepted text | 96.4 % | **3.6 %** |
| glm_ocr | 72.1 % | **0 %** |
| scribe-vlm | 90.3 % | 29.2 % |

The new rule's 78 aborted blocks, classified: **63 are verbatim loops
already stored in accepted markdown** (e.g. "to be used to be used to be
used…", "The study was conducted in a controlled environment." ×4); 8 are
assembled HTML tables (not a production input, see §2); 7 other, mostly
loops visible beyond the classifier's 60-word window.

Recall on loops built from the real blocks (14,124 cases per shape; caught
after the block's own text; mean loop words before the abort):

| loop shape | old rule | new rule |
|---|---|---|
| 2-word induction loop | 88.7 % (5.9 w) | **98.8 %** (29.2 w) |
| 3-word induction loop | 89.2 % (7.0 w) | **99.4 %** (28.7 w) |
| last sentence repeated (Nougat) | 88.3 % (22.9 w) | **95.6 %** (47.7 w) |
| paragraph replayed (≤ 120 words) | 88.4 % (81.6 w) | **94.3 %** (199.7 w) |

The old rule's lower recall is the same defect from the other side: it
had already fired on the block's own text, so the loop never got judged.
The new rule's misses are blocks whose own text already ended in a loop
(fired before the loop under test, counted as a miss).
