# Plan — the repetition detector fires only on consecutive loops

## Design

`RepetitionDetector` (`crates/hs-scribe/src/ocr/repetition_detector.rs`)
keeps, per period `p` in 1..=128, `run[p]`: how many consecutive newest
words equal the word `p` before them. The tail is `p`-periodic over
`run[p] + p` words. `check()` returns the shortest `p` with
`run[p] + p >= max(32, 3·p)`, i.e. at least 3 back-to-back copies of a
unit, spanning at least 32 words.

- State: a 128-slot ring of word hashes, 129 counters. Fixed size, no
  heap; a word is hashed while it is normalized (lowercase letters and
  digits), so no string is built.
- Cost: O(128) per word, O(128) per check.
- Interface unchanged: `new`/`default`, `feed`, `check`, `LoopReason`
  (period 2 → `bigram`, 3 → `trigram`, else `n_gram(p)`), so diag JSONL
  and the wire failure code keep their form. `window_len`,
  `WINDOW_CAP_WORDS` and the configurable cap are gone (their only
  callers were tests).
- Callers unchanged: the streaming OpenAI-compatible client
  (`ocr/openai_compatible.rs`) aborts a region when `check()` is `Some`;
  the processor drops the region and the QC gate rejects the conversion
  as gapped. Only *what* counts as a loop changes.

## Touched

- `crates/hs-scribe/src/ocr/repetition_detector.rs` — rule, docs, tests.
- `crates/hs-scribe/README.md` — the rule, in "Repetition loops and gaps".
- `BACKLOG.md` — P0-28 (re-ingest the dropped documents, needs a
  decision), P1-29 (verbatim loops already stored), P1-30 (bmb autotune
  crash loop and unbounded logs); annotation on the old "4-gram-cycle"
  note.

## Tests

- Must fire: 2-word loop (bigram), 3-word loop (trigram), a 14-word
  sentence replayed, at the third copy and not the second; case and
  punctuation insensitive; period 128 fires, 129 does not.
- Must not fire: the four passages the old rule aborted on (affiliation
  list, citation run, repeated key phrase, dense topical prose); short
  legitimate repetition ("very very very", a row of zeros, one duplicated
  phrase); a loop that ends stops firing.
- Oracle: incremental runs agree with a direct scan of the whole stream at
  every word, on 4 seeded streams with injected loops.
- No allocation in `feed`/`check` (counting allocator).

## Rollout

Ships in rc.385; takes effect on a scribe server once `hs upgrade` runs
there (operator). The tier that matters is bmb's glm_ocr. Documents
already dropped stay dropped until the operator decides P0-28.

## Risk

- A loop that is not verbatim (words drift each copy) or longer than 128
  words is no longer aborted mid-stream; it runs to the token limit and
  is cleaned by `clean_repetitions`. That was the behaviour before the
  streaming detector, and Nougat notes no strict detector catches it.
- Aborts happen later: ~29 words into a 2-word loop instead of ~6. Cost:
  ~23 extra tokens per real loop on a backend generating ~8k per region.
