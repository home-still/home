# How conversion works

Conversion turns a source document (PDF, HTML or EPUB) into markdown that
keeps the source's pages, headings, tables and formulas. It is the stage that
decides how good everything after it can be: a passage the converter misread
is a passage search cannot find.

## PDFs: layout first, then a model per region

A PDF is a picture of text. Reading it top to bottom, as plain OCR does,
mixes columns, splits tables into nonsense and loses equations. The scribe
server's default converter therefore works out the page's structure before it
reads anything:

1. **Render.** pdfium renders each page to an image (200 DPI by default).
   pdfium also counts the pages of every PDF before it is dispatched, so a
   document's length is known before any work starts.
2. **Find the regions.** A layout model, PP-DocLayout-V3, finds the regions on
   the page (titles, paragraphs, tables, display and inline formulas,
   figures, captions, references, footnotes, headers and footers) and their
   reading order.
3. **Find the table cells.** For each table, a table-structure model,
   SLANet-Plus, finds the cell boundaries.
4. **Read each region.** Each region goes to a vision-language model, GLM-OCR
   by default, with a prompt for its kind: text, formula, or one table cell
   at a time. Headers, footers and page numbers are left out; figures become
   placeholders.
5. **Assemble.** The regions are put in reading order and written as markdown
   with a heading hierarchy, HTML tables and formula blocks. Pages are joined
   by a separator reserved for that purpose, so page *N* of the markdown is
   page *N* of the source; search hits and document links rely on it.

Reading a region at a time, and a table a cell at a time, costs more model
calls than reading a whole page, but each call sees exactly one thing with
the right instruction, and the table comes out with its structure intact.
`HS_SCRIBE_PIPELINE_MODE=FullPage` skips the layout step and sends whole
pages instead: faster, with no table structure.

The two layout models are ONNX files the server loads from
`~/.home-still/models/`; the VLM runs in a backend you choose: Ollama, any
OpenAI-compatible server (llama-server, llama-swap, vLLM), or a cloud API.
See the [scribe server settings](../reference/configuration.md).

### The olmocr converter

`HS_SCRIBE_CONVERTER=olmocr` hands each PDF to the olmOCR command-line tool
against a model served by an OpenAI-compatible endpoint; it renders and reads
whole pages itself. The server cuts olmocr's text at olmocr's own page
boundaries, so its markdown keeps its pages like the default converter's. A
result that does not account for every page is refused rather than stored
without them.

Because the two converters fail on different documents, a cluster commonly
runs both as [tiers](architecture.md#the-scribe-tier-chain): olmocr first, and
the region pipeline for the documents it cannot read.

## HTML and EPUB

HTML pages and EPUB chapters are already text, so they go through a parser,
not a model. Every outermost `<article>` is converted in document order;
navigation, sidebars and other asides are dropped (EPUB 3 footnotes are
kept). Every parse is bounded: input size, nesting depth, node and attribute
counts, and wall-clock time. A document over a bound fails; nothing is
silently truncated.

## Quality gates

A conversion is stored only if it is complete. Three checks enforce that.

**The repetition detector.** A vision-language model sometimes falls into a
loop and repeats the same words until it reaches its token limit. Once a loop
starts it does not end by itself, so the server watches each region's output
as it streams and aborts the region when the tail of the stream is at least
3 consecutive copies of one unit of 1 to 128 words, together at least 32
words long. Only a *consecutive* loop counts: an affiliation list, a run of
citations, or a paper repeating its key term is normal text and is left
alone. Since one aborted region costs the whole conversion, the detector
waits for an unambiguous signal, which a real loop always produces.
Near-copy loops, which no strict detector can catch, run to the token limit
and are removed by a cleanup pass on the finished page.

**No gaps.** A region that could not be read, whether aborted by the
detector or skipped for another reason, leaves a hole in the page. A
conversion with any hole is rejected as `gapped_conversion` instead of being
stored with text missing.

**Not empty.** A conversion that yields less text than can be indexed is
rejected: `empty_vlm_conversion` when a model produced it (another model may
do better), `empty_conversion` when the HTML or EPUB parser did (every tier
would produce the same).

## When a conversion fails

Every failure is recorded in the document's catalog entry as a reason, and
each reason is one of two kinds.

- **Retryable**: a backend failed on the document. The model looped, its
  stream broke, it stopped at its token limit, it left gaps, or olmocr lost
  pages. A different model, or the same one later, may succeed. These
  failures escalate through the tiers first, and are stamped only when every
  tier has failed.
- **Bad source**: the document cannot convert anywhere. It is an HTML page
  saved as `.pdf`, an encrypted or broken PDF, a paywall page, or a request
  the model refuses outright. Retrying wastes GPU time, so these are stamped
  at once and left out of the dashboard's conversion progress.

This split is why two recovery commands exist. `hs pipeline catch-up` queues
documents that have no markdown *and no failure stamp*: work that was never
queued or whose message was lost. It skips failed conversions on purpose, so
running it repeatedly never feeds the same failing documents back in.
`hs pipeline reconvert-failed` is the deliberate retry: it re-queues every
retryable failure by default, after you have fixed what caused them (added a
tier, freed the GPU), and re-queues bad sources only when you name their
reason with `--reason`. Both take `--dry-run` to show what they would queue.
The reasons and their kinds are listed in
[conversion failure reasons](../reference/status-dashboard.md#conversion-failure-reasons).
