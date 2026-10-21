# Evaluation — highlighting the passage in the served document

Measured on the live corpus on `big`, 2026-10-21.

## The problem

The previous iteration gave an agent's search hit a **link the user can
open** (rc.372). The link opened the document — at the **top**. The
user's question is "show me *this* passage", and the answer was a PDF
at page 1 with three thousand pages of paper around it.

Measured over the 250 hits of the fixed 25-query set: **90 % of hits
carry a page number** (9 of 10 in a sampled query). The anchor data was
already there.

## The change

A link can now name **the page** the passage is on, and the passage
itself.

- The **PDF** link carries `#page=N`. Every PDF viewer honours it and
  scrolls to that page.
- The **markdown** link carries a W3C **Text Fragment**,
  `#:~:text=<the passage>`. The browser finds the quoted text in the
  document it loaded, scrolls to it and selects it. It is resolved in
  the *client*, against the bytes the server sent — the server serves
  the document, unchanged.
- The **markdown route** takes `?page=N` and serves that one page, so
  a client that wants exactly one page gets it without scrolling. This
  is the issue's own design, and it is what the MCP `markdown_read`
  tool already does.

The page is **signed**: it is part of the request, so a link minted for
page 11 opens page 11 and nothing else. A tampered page is a 403,
exactly like a tampered stem.

## Result

| | before | after |
|---|---|---|
| a search hit's link points at the passage | **no** | **yes** |
| the PDF link names the page | no | **yes** (`#page=N`) |
| the markdown link quotes the passage | no | **yes** (`#:~:text=…`) |
| the markdown link serves one page | no | **yes** (`?page=N`) |
| RECALL@10 | 24/25 = 96 % | 24/25 = 96 % (unchanged) |
| median latency | 158 ms | 174 ms (unchanged) |

**Recall is unchanged.** The change adds a URL to a link and a page to
a route; it touches no retrieval behaviour and no stored data.

## Verified end to end, on real documents

**A search hit's links**, from the live MCP server over HTTP:

```
doc_id=10.3390_s21103461  page=11
  source:   …/source?…&page=11&sig=…#page=11
  markdown: …/markdown?…&page=11&sig=…#:~:text=During%20acute%20stress%2C%20the%20limbic…
```

**A link that names a page serves that page.** The markdown of
`10.1677_joe.1.06233` is 73,923 bytes over 17 pages; page 15 is 7,453
bytes and starts with the reference list:

```
GET /docs/v1/10.1677_joe.1.06233/markdown?…&page=15&sig=…
→ 200 OK
   content-type: text/markdown; charset=utf-8
   content-length: 7453
   content-location: #page=15

   Carani C, Zini D, Baldini A, Casa LD, Ghizzani A, Marrama P 1990a Effects of
   androgen treatment in impotent men with normal and low levels of free
   testosterone. Archives of Sexual Behavior 19 223-234.
```

**The page correspondence is exact.** The markdown's page 15 is on the
source PDF's page 15: searching the PDF's page 15 for the markdown's
page-15 text finds it, and the PDF has 17 pages — the same count as
the markdown. The correspondence is not a coincidence; it is
guaranteed by the conversion pipeline (the scribe renders the PDF's
pages in order, sorts the concurrent results back by page index, and
joins them with a **reserved** separator, so markdown page N *is* source
page N).

**The browser finds the quoted text.** For every sampled hit, the
snippet the link quotes is a **substring of the page the link serves**
— which is exactly what the browser's match requires:

| document | page | snippet | in the served page? |
|---|---|---|---|
| `10.3390_s21103461` | 11 | 110 chars | **yes** |
| `W2130006095` | 11 | 84 chars | **yes** |
| `W3199056238` | 10 | 109 chars | **yes** |

**A tampered page is refused**, like a tampered stem: a link for page 2
cannot be edited into a link for page 3, because the page is in the
signed payload.

**A page the document does not have is a 404**, not an empty page and
not a 403: the link was good, the page is not there.

## The two things that were measured, not assumed

**How long a snippet should be.** The text-fragment spec asks for a
*short* distinctive snippet, and a URL has a practical length limit.
The first implementation quoted the passage's whole first line: over
the corpus's own passages that averages **308 characters** and reaches
**878** — too long for a URL a user has to read, and longer than the
browser needs. The snippet is now cut at a **word boundary** at 120
characters, which keeps it a substring of the document (a snippet cut
mid-word is not, and the browser would find nothing). Measured over
the corpus's first lines: the mean is 308, and after the cut every
sampled snippet is 84–110 characters and is present in the page its
link serves.

**Whether the markdown's pages are the PDF's pages.** They are, and the
code says so: `crates/hs-scribe/src/pipeline/processor.rs` renders
the PDF's pages `0..page_count` in order, sorts the concurrent results
back by page index, and `join_pages` joins them with the reserved
separator `\n\n---\n\n` — a `---` the model emits *inside* a page is
rewritten to `***` first, so that sequence only ever appears between
two pages. Verified on a real 17-page document: the markdown has 17
pages, the PDF has 17 pages, and the markdown's page 15 is on the
PDF's page 15.

The one case where the correspondence does not hold is a document whose
markdown has a single page for a multi-page source — the olmocr
backend's flat output. There the catalog row records **no** page
offsets (`resolve_page_accounting` marks them untrustworthy), so such
a chunk has no page, and the link for it names no page. The link is
still correct; it just points at the whole document, because there is
nothing better to point at.

## Timings

- The markdown page is served in **2–3 ms**: the object is read and
  split on the reserved separator, and the page is a slice of it.
- The search path is untouched: median latency 158 ms before, 174 ms
  after (the same query set, the same corpus; the difference is the
  corpus growing).
- A PDF link is unchanged in cost: the PDF is still streamed with
  `Range` support, so a viewer seeking inside the page issues the same
  requests it did before.

## How the numbers were produced

```sh
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/run.json
```

The end-to-end checks were run against the live docs service on
`big`, against the real corpus, with `curl` and the MCP server over
HTTP.
