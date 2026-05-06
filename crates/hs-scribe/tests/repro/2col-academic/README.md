# 2-column academic PDF repro corpus

Pinned PDF fixtures + golden markdown for the `repro_2col_test` integration
test (`crates/hs-scribe/tests/repro_2col_test.rs`). Exists so the next
GLM-OCR repetition incident is caught by CI before it hits production.

## Why this corpus

Repetition loops reproduce most reliably on 2-column dense academic text
(Nougat 1.5%+ on in-domain pages, worse OOD; see the project analysis doc
that drove rc.312). A fixed corpus of known-bad layouts gives:

- Deterministic regression coverage for Phases A/B/C and any future
  sampling-param tuning.
- A baseline against which to validate `--diag` JSONL output.
- A failure signal on backend changes (vLLM/SGLang/mlx-vlm migration,
  Ollama upstream patches) before they touch live papers.

## Pinning policy

The test's golden markdown is captured against a specific backend at a
specific model version. **Regenerate goldens whenever any of the following
change:** scribe binary version, OCR backend (Ollama vs vLLM vs mlx-vlm),
GLM-OCR model digest, sampling params (top_k / repeat_penalty /
no_repeat_ngram_size), DPI. Pin all of these in this README each time
you commit a new golden so future drift is auditable.

Current pin:
- scribe binary: `<TODO: rc.NNN>`
- backend: `<TODO: openai-compat (vLLM) | ollama>`
- VLM model + digest: `<TODO: glm-ocr@sha256:...>`
- DPI: `<TODO: 200>`
- Sampling: see `crates/hs-scribe/src/ocr/openai_compatible.rs::build_request_body`
- Captured at: `<TODO: 2026-MM-DD>`

## Adding a fixture

1. Pick an arXiv 2-column paper that exercises the failure modes listed
   below. Avoid copyrighted publisher PDFs unless a redistribution
   license allows it; arXiv's CC license is fine.
2. Drop the PDF here as `<short-stem>.pdf`. Keep stems short — they
   become the `.diag.jsonl` filename.
3. Record the source URL and SHA-256 in the table below.
4. Run the conversion end-to-end against a working cluster:

   ```bash
   HS_SCRIBE_REPRO=1 \
   HS_SCRIBE_TEST_BACKEND=openai-compat \
   HS_SCRIBE_TEST_SCRIBE_URL=http://192.168.1.110:7433 \
   HS_SCRIBE_DIAG_DIR=/tmp/repro-diag \
   cargo test -p hs-scribe --test repro_2col_test -- --ignored --nocapture
   ```

   The first run with no golden present prints the captured markdown to
   stderr — review it, then save it as `golden/<short-stem>.md`.
5. Commit fixture + golden + this README's pin block.

## Wanted failure modes

- **Bibliography-heavy survey** (`paper-arxiv-bib-heavy.pdf`): ≥4 pages
  of references; exercises the `QC_BIBLIOGRAPHY_MULTIPLIER` (Phase C).
  Source candidate: a recent arXiv survey on transformer architectures
  or RAG.
- **Mixed single/double-column with display math**
  (`paper-mixed-col-math.pdf`): NeurIPS/ICML-style template with
  single-column abstract + double-column body + several display
  formulas. Catches both column-detection and formula-region routing.
- **(optional) Citation-list torture page** (`paper-citation-torture.pdf`):
  one or two pages with extreme citation density; included if regular
  fixtures aren't tripping any safety gates and we want signal.

## Fixtures

| Stem | Source URL | SHA-256 | Pages | License |
|------|------------|---------|-------|---------|
| TODO | TODO | TODO | TODO | TODO |

(Populate as fixtures are added. The test reads SHA-256 lines from this
file at runtime — keep the table format stable so the parser doesn't
need updating.)

## What "passes" means

For each fixture, all of:
- `qc_verdict == Accept`
- Sum of per-page `TruncationCounts.total()` is 0
- `levenshtein(actual, golden) / golden.chars().count() < 0.005`
  (tolerates whitespace/punctuation jitter; rejects a single hallucinated
  paragraph or dropped column)

## Why Ollama is rejected as a test backend

`ollama#14493` / `#10767`: the Go VLM runner silently drops
`repeat_penalty` / `frequency_penalty` / `presence_penalty`. A test that
passes against Ollama tells us nothing about the sampling params we
actually configured — it's measuring the model's untuned behavior. The
test panics with a pointer to the issues if `HS_SCRIBE_TEST_BACKEND=ollama`.

vLLM via the OpenAI-compatible backend is the right target: penalty
params fire as configured, and it's the supported production path on
Linux+CUDA per the analysis doc's prioritized action plan.
