# Research — olmocr conversions lose their page numbers

**Date:** 2026-10-22
**Question:** why does more than a quarter of search hits carry no
`page`, so the document link opens page 1 instead of the page the
passage is on — and what is the smallest change that stops it?

---

## 1. The symptom, measured

Every search hit carries a `links.source` URL. When the hit has a
`page`, that URL ends in `#page=N` and a PDF viewer opens on the
passage's page; when it does not, the viewer opens page 1.

| measurement (live corpus, 2026-10-22) | value |
|---|---|
| search hits with a `page` (80 hits, 8 queries) | **72.5 %** |
| search hits **without** a `page` | **27.5 %** |
| markdown objects with page separators (60, seeded sample) | 41.7 % |
| catalog rows with a usable page map (120, seeded sample) | 45.0 % |
| catalog rows with a page *count* but **no** map | 36.7 % |

## 2. The cause, attributed (not assumed)

Cross-tab of converter × markdown shape over a seeded 100-row sample:

| converter | separated | flat |
|---|---|---|
| **olmocr** | **0** | **39** |
| scribe-vlm (layout pipeline) | 25 | 2 |
| glm_ocr | 7 | 1 |
| html-parser | 0 | 4 (HTML has no pages; correct) |

**Every olmocr conversion is flat; no other converter is.** The
catalog's page map comes from `compute_page_offsets`, which splits the
markdown on the reserved `\n\n---\n\n` separator; with no separator,
`resolve_page_accounting` (correctly) refuses a one-span map for a
multi-page PDF, so no chunk of the document gets a page.

### olmocr already knows the pages

olmocr's pipeline (`olmocr/pipeline.py`, `build_dolma_document`, in the
version installed on `big`) builds its output document page by page and
records, for every page, `[start, end, page_num]` in
`attributes.pdf_page_numbers`. It writes that document to
`results/output_<hash>.jsonl`. Its `--markdown` option then writes
`doc["text"]` — the same text, pages joined by a single `"\n"` — to a
`.md` file. **Our converter read only the `.md` file**, so the page
boundaries olmocr computed were discarded.

Verified on a real run (6-page PDF, olmocr on `big`, 109 s):

- one result document; spans `[[0,2795,1], [2795,8699,2], … ,
  [26366,33420,6]]` — contiguous, in order, ending at the text's end;
- the `.md` file is byte-identical to `text`, with 0 page separators;
- **the spans are character positions, not bytes**: the text is 33,420
  characters and 33,602 bytes (95 non-ASCII characters). Reading the
  spans as byte offsets would misplace every page boundary after the
  first non-ASCII character.

## 3. Literature

- **olmOCR** — Poznanski et al., *olmOCR: Unlocking Trillions of Tokens
  in PDFs with Vision Language Models*, 2025,
  doi:10.48550/arxiv.2502.18443. Added to the corpus this iteration
  (`hs paper download --doi`); it is queued for conversion. The format
  facts above are taken from the installed pipeline's source and a real
  run, not from the paper.
- **Grounding needs a location.** *Retrieval-Augmented Generation
  (RAG)* in the corpus: "Providing valid references to the generated
  result has been termed grounding (Magesh et al. 2024) … it gives a
  user additional information about where the information comes from."
  A reference that opens page 1 of a 30-page paper is a weaker reference.
- *An auditable and source-verified framework for clinical AI decision
  support: integrating retrieval-augmented generation with data
  provenance* (Front. Artif. Intell. 2026, doi:10.3389/frai.2026.1737532),
  in the corpus: provenance is what makes a retrieved claim auditable.
- *A Philosophy of Software Design* (Ousterhout): one path. The page
  boundaries have exactly one authoritative source here — the converter
  that rendered the pages — so the fix reads them from there rather than
  reconstructing them heuristically from the PDF text layer.

## 4. Technique chosen

Read olmocr's result document instead of its flattened markdown file,
cut the text at `pdf_page_numbers` (converting character positions to
byte offsets in one pass), and join the pages with `join_pages` — the
project's one page joiner, which also defends the reserved separator.
Validate the spans tile the whole text, in page order, for exactly the
PDF's page count; anything else is refused as an incomplete olmocr run
(escalated to the next backend), never stored as a degraded document.

### Rejected alternatives

| alternative | why not |
|---|---|
| Recover pages for flat markdown by aligning with the PDF text layer | a heuristic second path; image-only and code-dense PDFs (olmocr's reason to exist) have weak text layers |
| Keep `--markdown` and also read the JSONL | two sources for one text; the `.md` adds nothing once the JSONL is read |
| Re-convert the existing flat corpus | forbidden (no corpus rebuilds); the fix applies to every conversion from now on |

## 5. Expected effect

- Every **new** olmocr conversion stores one markdown page per PDF page;
  the catalog gets a trustworthy page map; every chunk gets a `page`;
  every hit's source link carries `#page=N`.
- Existing flat documents are unchanged (re-conversion is out of scope).
  The backlog of ~1,800 queued documents converts with pages.
- Search recall on the fixed query set must not move: no search code
  changes.
