# Run 2026-10-21-05 — highlighting the passage in the served document

## Problem, measured

The previous iteration gave an agent's search hit a **link the user can
open** (rc.372). The link opens the document, but it opens it at the
**top**. The user's question is "show me *this* passage", and the answer
is a PDF at page 1 with 3,000 pages of paper around it.

The wishlist says it in one line: *"a really cool feature would be able
to highlight the text in the document when all this needs to be served
remotely."* That is the second half of issue #7, and the first half is
done.

Measured on the live corpus, over the 250 hits of the fixed 25-query
set: **90 % of hits carry a page number** (9 of 10 in a sampled query).
The anchor data is already there — it has been there all along.

## What the code already guarantees

Read, not assumed (the scout's report is in this run's notes; the
decisive facts are):

1. **The converted markdown's pages are the source PDF's pages.**
   `crates/hs-scribe/src/pipeline/processor.rs` renders PDF pages
   `0..page_count` in order, sorts the concurrent results back by page
   index (`collected.sort_by_key(|(i, _)| *i)`), and joins them with
   `join_pages` (`crates/hs-scribe/src/pipeline/markdown_generator.rs`).
   The join is the **reserved** separator `\n\n---\n\n`; a `---` a model
   emits inside a page is rewritten to `***` first, so an in-page rule
   cannot create a false page break.

   So **markdown page N is PDF page N**, by construction.

2. **The catalog row records the page map.**
   `conversion.pages: Vec<PageOffset>` (`hs-common/src/catalog.rs:106`),
   each entry `{ page, char_start, char_end }` — the page's byte range
   in the markdown, computed by `compute_page_offsets` from the same
   separator. It is written by scribe at conversion time and is dropped
   (`Vec::new()`) when it cannot be trusted: `resolve_page_accounting`
   sets `offsets_trustworthy` false for a single-page markdown from a
   multi-page source (the olmocr flat blob).

3. **A chunk's page is that map applied to the chunk's start byte.**
   `crates/hs-distill/src/chunker.rs:66`:
   ```rust
   fn resolve_page(page_offsets: &[PageOffset], char_start: usize) -> Option<usize> {
       page_offsets.iter()
           .find(|po| char_start >= po.char_start && char_start < po.char_end)
           .map(|po| po.page)
   }
   ```
   A chunk cannot cross a page: `chunk_markdown` splits at the
   separator first.

4. **A chunk's line numbers are whole-file markdown line numbers.**
   `line_start` / `line_end` are 1-based, inclusive line numbers of the
   full markdown file, from `build_line_offsets` over the whole text.

So a hit carries (`line_start`, `line_end`, `page`), and the *document*
carries the page map. Everything a highlight needs is in the corpus
already; nothing has to be re-indexed.

## Techniques considered

**A. A PDF `#page=N` fragment on the source link.** The simplest thing
that could work, and it needs nothing new: the fragment is a URL
fragment, it is never sent to the server, and every PDF viewer honours
it. **It only works for the PDF.** The markdown link, the HTML source
and the EPUB source get no anchor, and for the PDF it opens the *page*,
not the passage — a 3,000-page book still puts the user in the right
neighbourhood, not on the line.

**B. A markdown text fragment.** The W3C **Text Fragments** spec
(`#:~:text=`) is the browser-native way to point at a span of text: the
browser finds the quoted text in the loaded document and scrolls to it,
selecting it. Chrome, Edge and Safari support it; Firefox has it
behind a flag. It needs no server-side support at all — the fragment is
resolved in the browser against the document the server sent.

**C. A server-side highlight: the server injects the markup.** The
docs service reads the markdown, finds the passage, wraps it in
`<mark>` and serves the result. Exact, and it works for every client.
But it changes the bytes the server serves, and the served document is
also what an agent reads — a server that rewrites the document serves
two different things under one URL, and the agent's copy and the
user's copy stop being the same text.

**D. Both A and B, and a page route.** `#page=N` for the PDF, `#:~:text=`
for the markdown, and a `?page=N` query on the markdown route so a
client that wants exactly one page can have it without scrolling.

### The choice: **D**

- **The PDF gets `#page=N`.** It is the whole feature for a PDF, it is
  free, and every viewer honours it.
- **The markdown gets `#:~:text=`.** It is the browser-native way to
  point at a span, it survives the user scrolling, and it highlights
  *the passage* rather than the page — which is what the user asked
  for.
- **The markdown route gets `?page=N`.** The issue's own design lists
  it (`GET /docs/v1/<stem>/markdown?page=N`: one page), and it is what
  the MCP `markdown_read` tool already does with its own `page`
  argument — one implementation, one rule.

**C is rejected on a rule, not a taste:** the server must serve the
document, and a server that rewrites what it serves cannot also be the
thing an agent reads. The highlight belongs in the *client*, and the
mechanism the clients already have for that is the URL fragment.

## What the signature has to cover

A link that names a passage must not be usable to open a *different*
passage. The signature already covers `stem`, `fmt`, `sub`, `iat` and
`exp`. Adding the page to the signed payload means:

- a link minted for page 11 of a document only opens page 11;
- changing the page in the URL invalidates the signature.

The page is part of the *request*, not part of the *document*, so it
belongs in the signature — the same reason `fmt` is there. A fragment
(`#page=11`) is never sent to the server, so it cannot be signed
directly; the page therefore travels in the **query string** (signed)
and the fragment is a *pointer* to it.

That means one link, one page: `?page=11#page=11`. The query is what
the server checks; the fragment is what the browser uses. And for the
PDF, the fragment alone is enough — but the server must still know
which page, so the query carries it too.

## The text fragment

The text fragment's payload is the passage's text, URL-encoded. A
passage can be a hundred lines, and a URL has a length limit. The
spec's own guidance: the fragment quotes a **short** distinctive
snippet, not the whole passage. So the fragment quotes the **first
line of the passage** — one line, which the browser matches
case-insensitively after normalizing whitespace.

The first line is the right choice for a second reason: the chunk's
`line_start` is the passage's first line, so the service can find it by
*line number* without searching the text at all.

## Expected effect

- A user who follows a search hit's link lands **on the passage**:
  the PDF at the right page, the markdown with the passage selected and
  scrolled into view.
- The link still names exactly one document, one format, one page and
  one subject, and it still expires.
- A tampered page is refused, exactly like a tampered stem.
- No re-index, no re-embed, no change to any vector.

## Constraint compliance

- No corpus rebuild, no purge, no re-embed, no change to the durable
  consumers.
- The change is a *URL* change: what the service serves is unchanged,
  and the fragment is resolved in the client.
- The signature rule gains one field; the key and the file format do
  not change.
- The MCP tools gain a `page` on the link, and the tools stay
  report-only.
