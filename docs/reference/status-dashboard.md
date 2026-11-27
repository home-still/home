# `hs status` dashboard

`hs status` shows the pipeline's counts, the services it can reach and the
recent activity. In an interactive terminal it is a live dashboard (`q`, Esc or
Ctrl+C quits); when stdout or stdin is not a terminal it prints the same data
once as text. `--output json` and `--output ndjson` print it once as JSON.

The data comes from the MCP server's `GET /mcp/status` route (see
[HTTP routes](mcp-tools.md#http-routes)). An agent reads the pipeline with the
[`status`](mcp-tools.md#status) tool. `hs status` reaches the MCP server in one
of two ways:

| Setting | Route |
|---|---|
| `HS_MCP_URL` set, e.g. `HS_MCP_URL=http://localhost:7445/mcp` | Directly to that URL, sending `HS_BACKEND_TOKEN` when it is set. |
| `HS_MCP_URL` unset | Through the cloud gateway with the credentials saved by `hs cloud enroll`. |

## Pipeline rows

| Row | Count | Detail | Progress | Act on it with |
|---|---|---|---|---|
| Documents | Source documents (PDF, HTML, EPUB) under `papers/`. | Total size. | | |
| Markdown | Markdown documents under `markdown/`. | Total size. | Markdown ÷ (Documents − bad sources), capped at 100 %. A document that failed for a retryable reason stays in the denominator. | `hs pipeline catch-up` |
| Cataloged | Catalog entries. | | | `hs scribe catalog-backfill` |
| Embedded | Documents with records in the store. | Chunk count, and how many documents are stamped `embedding_skip` (markdown that yields no chunk). | Embedded ÷ (Markdown − skipped). | `hs distill reconcile --reembed` |
| In-flight | Conversions running now, summed over every scribe server's `/readiness`. | `converting across N scribes`, or `idle`. | | |
| Queued | Messages outstanding on the `scribe-workers` consumer: waiting in the stream plus delivered and not yet acknowledged. | `waiting for or held by a scribe`, `queue empty`, or `unavailable: <reason>` when the broker cannot say. `?` means unknown, never zero. | | |
| Stalled | Sources with no markdown and no failure stamp, minus Queued. These were never queued, or their message was lost. | `no markdown and not queued — run hs pipeline catch-up`, or `none`. | | `hs pipeline catch-up --dry-run` |
| Inbox | Files waiting in `papers/manually_downloaded/`. | `pending in manually_downloaded/ (N)` or `manually_downloaded/ empty`. | | `hs scribe inbox sweep` |
| Failed, retry | Documents stamped `conversion_failed` with a retryable reason. | `hs pipeline reconvert-failed --dry-run` when the count is above zero, else `none`. | The largest retryable reason and its count, then `N bad source` and `N unclassified` when present. | `hs pipeline reconvert-failed --dry-run` |
| Watcher | The inbox daemon (`hs scribe inbox run`), from the heartbeat it writes on every tick. | `running · <host>`, plus `swept <moved> / <found>` after its first sweep; or `stopped`. | `last tick Ns ago`, plus `K err` when the last sweep had errors. | `hs scribe inbox status` |

The text output prints the failed conversions in full:

```text
Failed    : 2032 retryable — recover with `hs pipeline reconvert-failed --dry-run`
            gapped_conversion 1583, vlm_transport_error 432, permanent_convert_failure 17
Bad source: 155 (empty_conversion 122, paywall_html 33)
```

A row reads `n/a` when the MCP server could not count (the panel title then
names the cause), and `...` while the first count is in progress.

## Services rows

One row per scribe server in `scribe.servers`, one per distill server in
`distill.servers`, and a last one, `Store`, for the myelin server the distill server uses.

| Column | Meaning |
|---|---|
| Indicator | `●` healthy, `○` not. |
| Status | `running`, `idle`, `N converting` (scribe), `N embedding` (distill), `healthy`/`stopped` (the store), or the reason the service is unhealthy. |
| URL | The configured address; for the store row, the myelin server's URL. Never truncated. |
| Detail | Model, slots and device as the service reports them. For the store row: the store's name. |
| Version | The service's version. |

When a scribe server's VLM backend lacks GPU memory, the status is a
diagnosis rather than a label, for example:

```text
backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by pid=4242 7534MB hs-distill-server; …
```

It names the free memory, the headroom the backend needs, and the processes
holding the card. See
[Sharing one GPU](../explanation/architecture.md#sharing-one-gpu).

## Conversion failure reasons

A document whose conversion fails is stamped `conversion_failed: <reason>` in
the catalog. The reason's kind decides what can recover it:

- **retryable**: a backend failed on the document. A different or later
  backend may succeed; `hs pipeline reconvert-failed` re-queues these by
  default.
- **bad source**: the bytes cannot convert on any backend. They are left out
  of the Markdown progress denominator. `hs pipeline reconvert-failed --reason
  <reason>` re-queues them only when you name the reason.
- **unclassified**: any other token. It is never guessed into a kind.

| Reason | Kind | Meaning |
|---|---|---|
| `unsupported_content_type:html` | bad source | A `.pdf` object that is actually HTML. |
| `unsupported_content_type:binary` | bad source | A `.pdf` object that is not a PDF. |
| `paywall_html` | bad source | HTML that is a paywall or loading page. |
| `pdf_parse_error` | bad source | The PDF is broken or encrypted, or has a page box or page count the renderer refuses. |
| `epub_parse_error` | bad source | The EPUB cannot be opened, exceeds the size caps, or has a missing, escaping or non-UTF-8 spine item. |
| `html_not_utf8` | bad source | The HTML is not UTF-8. |
| `html_parse_error` | bad source | The HTML nests too deeply, or its conversion did not finish in time. |
| `text_not_utf8` | bad source | The Markdown or plain-text source is not UTF-8. |
| `text_too_large` | bad source | The Markdown or plain-text source is over 16 MiB. |
| `unsupported_extension` | bad source | No converter handles the file's extension, or the store that holds the source does not take it. |
| `source_missing` | bad source | The source object does not exist in storage. |
| `missing_extension` | bad source | The event's key has no extension. |
| `invalid_key` | bad source | The event's key cannot name a document (escapes the storage root, an empty or dot-segment stem, or under no store's source prefix). |
| `empty_conversion` | bad source | The HTML or EPUB parser produced less text than can be indexed, or a Markdown or plain-text source holds less. |
| `conversion_panicked` | bad source | The server panicked converting this document; the same bytes panic again. |
| `vlm_request_rejected` | bad source | The VLM backend refused the page's request itself (bad request, payload too large, unsupported media). |
| `empty_vlm_conversion` | retryable | The VLM produced less text than can be indexed. |
| `vlm_repetition_loop` | retryable | The VLM repeated itself and the output was rejected. |
| `vlm_transport_error` | retryable | The VLM stream died mid-response. |
| `vlm_output_truncated` | retryable | The VLM stopped at its token limit. |
| `olmocr_zero_pages` | retryable | olmocr completed no page. |
| `olmocr_incomplete_pages` | retryable | olmocr failed pages, or completed fewer pages than the source has. |
| `gapped_conversion` | retryable | Regions of the document could not be processed, so the markdown has holes. |
| `permanent_convert_failure` | retryable | Every backend tier failed the document for a reason that was not named. |

The tokens are defined in `crates/hs-scribe/src/classify.rs`.
