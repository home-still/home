# MCP tools reference

`hs-mcp` is the Model Context Protocol server for home-still. This page lists every tool, resource and setting it exposes. To connect a client, see [Connect Claude and agents](../how-to/connect-claude-and-agents.md).

## Server

### Transports

| Mode | How to start | Notes |
|---|---|---|
| stdio (default) | `hs-mcp` | The process speaks MCP on stdin and stdout. Logs go to the spool, never to the terminal. |
| HTTP | `hs-mcp --serve <addr>` | Streamable HTTP MCP, for example `hs-mcp --serve 127.0.0.1:7445`. |

| Flag | Default | Meaning |
|---|---|---|
| `--serve <addr>` | none (stdio) | Run as an HTTP server on this address. |
| `--session-idle-timeout-secs <n>` | `3600` | HTTP only. Drop an MCP session after this many seconds without any message. Must be greater than zero and longer than the longest tool call that sends no progress notification. |

### HTTP authentication

The HTTP transport requires `HS_BACKEND_TOKEN`: at least 32 bytes of visible ASCII, set in the environment or in `~/.home-still/secrets.env`. The server refuses to start without it. Every request, on every path, must carry `Authorization: Bearer <token>`; a missing or wrong token gets `401`. There is no unauthenticated health route. The stdio transport needs no token.

### Startup requirements

| Requirement | Behaviour when absent |
|---|---|
| `storage:` section in `~/.home-still/config.yaml` | The server does not start. |
| `events:` section in `~/.home-still/config.yaml` | The server does not start. The broker connection is made on the first publish, so a broker outage affects only `paper_download`, `scribe_queue` and `scribe_convert`. |
| A malformed config section | The server does not start; the error names the section. |
| `scribe.servers`, `distill.servers` | The server starts. A tool that needs the missing service returns an error saying so. |

### Tool visibility

The five `openalex_*` tools appear in `tools/list` only when the OpenAlex database exists and holds the `openalex_works` row in its `_corpus_state` table, which `hs openalex build-fts` writes last. The check runs once at startup, so restart the server after building the corpus. See [Use the OpenAlex catalog](../how-to/use-the-openalex-catalog.md).

### What the tools cannot do

Destructive operations are CLI-only: deleting a personal document (`hs personal delete`), purging vectors (`hs distill purge`) and the `hs pipeline purge-*` commands. `catalog_repair`, `dedupe_url_encoded`, `distill_reconcile` and `distill_scan_repetitions` only report; a `dry_run` argument of `false` on them is ignored. See [CLI reference](cli.md).

### Prompts

| Prompt | Arguments | Purpose |
|---|---|---|
| `research_paper` | `topic` | Search papers, read documents and synthesise findings. |
| `summarize_document` | `stem` | Summarise one document from the library. |
| `compare_papers` | `stem_a`, `stem_b` | Compare two papers from the library. |

## Parameter conventions

| Type | Meaning |
|---|---|
| stem | A document stem: the file name without extension. A stem that is empty, absolute or contains a path separator or `..` is rejected with an invalid-parameters error before the tool runs. |
| string, integer, boolean | JSON string, number and boolean. |
| optional | The parameter may be omitted; the table gives the value used then. |

"Sends progress notifications" means the tool emits MCP `notifications/progress` messages while it runs, when the caller supplied a progress token: about every 20 seconds for most tools, and per batch of catalog reads for `catalog_repair`.

"Changes state: yes" means the tool's `read_only_hint` is `false`: it writes to storage, the catalog, the vector index or the event bus.

## Paper tools

### `paper_search`

Search academic papers across the configured providers. Returns a JSON object `{"papers": [...], "provider_failures": [...]}`. `provider_failures` names each provider that failed; if every provider fails the call is an error. Providers and their limits are listed in [Paper providers](paper-providers.md).

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `query` | string | required | Search query. |
| `max_results` | integer | `10` | Maximum results. |
| `search_type` | string | `keywords` | One of `keywords`, `title`, `author`, `doi`, `subject`. Anything else is an error. |
| `date` | string | none | Date filter, for example `>=2023` or `2020-2024`. A filter that does not parse is an error. |
| `offset` | integer | `0` | Result offset for paging. OpenAlex needs a multiple of `max_results`; Europe PMC cannot page by offset (an error when requested alone, left out of `all` when `offset` is above 0). |
| `provider` | string | `all` | One of `all`, `arxiv`, `openalex`, `semantic_scholar` (alias `s2`), `europmc` (alias `pmc`), `crossref`, `core`. Unknown values are an error. |
| `min_citations` | integer | none | Exclude papers with fewer citations. OpenAlex, Semantic Scholar and CORE honour it; arXiv does not. |
| `sort` | string | `relevance` | One of `relevance`, `citations`, `date`. Anything else is an error. |

### `paper_get`

Look up one paper by DOI. Returns JSON with the full metadata.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | DOI to look up. |

### `paper_references`

Return the structured reference list of a paper: each reference's DOI, title, year, authors, venue and citation count. Source: the Semantic Scholar Graph API.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | DOI of the paper whose reference list to return. |

### `paper_citations`

Return the papers that cite a paper (forward citation chaining), each with DOI, title, year, authors, venue and citation count. Source: the Semantic Scholar Graph API.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | DOI of the paper to find citing works for. |
| `limit` | integer | `100` | Maximum citations to return; maximum `1000`. |
| `year_from` | integer | none | Minimum publication year. Applied while paging, so `limit` counts only citing papers from this year on. |
| `sort` | string | `year` | `year` or `citations`. |

### `paper_download`

Download a paper's PDF by DOI into the papers directory. Tries arXiv, MDPI and the shared provider resolvers (Semantic Scholar, Europe PMC, CORE with a key, OpenAlex, CrossRef). Creates a catalog entry with metadata and announces the download on the event bus. Fails if the provider lookup or the catalog write fails; the PDF stays stored and a retry repairs the row. Returns JSON with the file path, size and SHA-256. Needs the event bus.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | DOI of the paper to download. |

## Catalog tools

### `catalog_list`

List papers in the catalog with title, conversion status and embedded status. Returns a JSON array.

Flag semantics:

| Field | Meaning |
|---|---|
| `converted` | `true` if and only if markdown exists. |
| `conversion_failed` | `null`, or `{reason, kind}` when the conversion was given up on. `kind` is `retryable` (a backend failed on the document; `hs pipeline reconvert-failed` re-queues it), `bad_source` (the source can never convert) or `unclassified`. Reasons are listed in [Status dashboard](status-dashboard.md#conversion-failure-reasons). |
| `embedded` | `true` only when Qdrant actually received chunks (`chunks_indexed > 0`). |

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `limit` | integer | all | Maximum items to return. |
| `offset` | integer | `0` | Items to skip. |
| `embedded` | boolean | all | `true` returns only embedded papers, `false` only papers not yet embedded. |

### `catalog_recent`

Most recent catalog activity: download, convert, embed and embed-skip events, one row per event, newest first. Ties break by stem. Rows whose timestamp comes from a `catalog_repair` backfill are left out unless `include_repaired` is `true`; those rows carry `"repair": true`. The status dashboard's history pane uses the same data.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `limit` | integer | `30` | Maximum items to return. |
| `include_repaired` | boolean | `false` | Include rows stamped by a `catalog_repair` backfill. |

### `catalog_read`

Read the full catalog entry for a paper: metadata, conversion info, page offsets and download URLs. Returns JSON.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Paper stem. |

### `cite`

Check a quote against the document it is from and return a citation a reader can verify. The quote is matched against the document's own text: markup, line breaks, spacing, hyphenation, case and typography may differ, the words may not. **A quote the document does not contain is a tool error** that names the closest passage, its lines and the share of the quote's words it holds; no citation is returned for it.

For a quote the document contains, the result is JSON:

| Field | Meaning |
|---|---|
| `store`, `doc_id` | What was asked. |
| `title`, `authors`, `year`, `doi`, `source_url` | From the document's catalog entry: `null`, or `[]` for `authors`, where the entry has none. |
| `quote` | `text` (the passage as the document has it), `exact` (`false` when the quote was found as the same words in the same order rather than as typed), `found_in` (`document` or `abstract`), `page`, `line_start` and `line_end`. `page` is the page the passage starts on: read from the word boxes when `boxes.status` is `located`, otherwise from the page offsets the conversion recorded for a PDF, otherwise `null`. The lines are lines of the converted text, and `null` for an abstract. |
| `boxes` | `status` is `located`, `not_located` (the word boxes are built and the passage could not be placed in them), `pending` (a PDF whose word boxes are not built yet) or `not_applicable` (the source is not a PDF, or the quote is from an abstract). `not_located` says the converted text and the words read from the PDF's pages disagree at the passage, so the quote is not confirmed on the page: in 100 sentences sampled from 20 converted papers (2026-11-25), all 5 that came back `not_located` were text the conversion had made up. `located` also carries `origin` (`text_layer`, or `ocr` when a page the passage is on was read by character recognition), `matched_ratio` (the share of the passage's words found on the page, in order) and `pages`: for each page the passage is on, `page` and `rects`, one `[x0, y0, x1, y1]` per line of the passage, in ten-thousandths of the page's width and height from its top left corner. |
| `links` | `source` (the original document, at the passage's page when it is known) and `markdown` (the converted text, the passage selected), each a `lan` and a `remote` URL. When `boxes.status` is `located`, also `viewer` (the whole PDF, opened on the boxed passage) and `page` (that page as a picture, the passage boxed). `original_url` is `source_url` with a text fragment that selects the passage, or `null`. `short` is `lan`, `remote` and `code` when `short` was asked for, otherwise `null`. `expires_at` is when the links stop working. |

With `image` the result has a second content item: the page the passage starts on as a PNG, the passage boxed. The MCP server fetches it from the docs service at `docs.lan_base_url`.

For `references` the quote is checked against the document the reference list belongs to, and for `abstracts` against the abstract in the document's catalog entry. Links are signed for the store that holds the document. The call fails when `docs.lan_base_url` or `docs.remote_base_url` is not set, when the signing key is unavailable, when the store or the document is unknown, when the document has no converted text yet, and for `memory`, which holds no documents. See [Open documents with links](../how-to/open-documents-with-links.md).

Changes state: only with `short`, which stores the code in the corpus.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `store` | string | `academic` | The store the passage was found in: the `store` of the hit. |
| `doc_id` | stem | required | The document's id: `meta.doc_id` of the hit. |
| `quote` | string | required | The passage, word for word: at least 4 words, at most 2000 characters. |
| `near_line` | integer | none | A line of the document near the passage. When the document says the quote more than once, the occurrence nearest this line is the one cited. Refused for `abstracts`. |
| `image` | boolean | `false` | Also return the page as an image with the passage boxed. The call fails unless `boxes.status` is `located`. |
| `short` | string | none | `viewer`, `page`, `source` or `markdown`: also store a 12-character code that redirects to that link, returned as `<base>/s/<code>` under `links.short`. The docs service answers the code with a redirect to the signed link, carrying its fragment, so the code expires with the link and is pruned by `hs docs prune`. `viewer` and `page` are refused unless `boxes.status` is `located`. Storing the code writes one small record to the corpus. |

### `catalog_repair`

Report catalog and storage disagreements in seven directions: forward disk orphans, `catalog_no_markdown`, `catalog_no_source` phantoms, flag drift, flag-drift resync, `md-path-drift` and stuck conversions. Report only: a `dry_run` of `false` is ignored and the response says so. `hs distill reconcile --fix-stamps --reembed` backfills embedding stamps; `hs pipeline catch-up` re-queues unconverted papers. Sends progress notifications as it reads the catalog.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `dry_run` | boolean | `true` | Ignored; the tool only reports. |
| `limit` | integer | none | Maximum number of orphans to examine in this call. |

### `dedupe_url_encoded`

Report URL-encoded duplicate stems: pairs of an encoded and a decoded form where the decoded twin is the one indexed. Report only.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `dry_run` | boolean | `true` | Ignored; the tool only reports. |

### `catalog_backfill_title`

Fill empty or missing `title` fields on catalog rows that have a DOI. Rows created by `catalog_repair`, the inbox watcher or server-event conversions have no title until a metadata lookup happens. For each eligible row the tool asks the aggregate paper provider and stamps `title`, plus `authors`, `publication_date`, `abstract_text` and `cited_by_count` when they come back. Each row costs one provider lookup. Sends progress notifications.

Changes state: yes (unless `dry_run` is `true`).

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `dry_run` | boolean | `true` | Report what would be backfilled without writing. |
| `limit` | integer | none | Maximum number of catalog rows to process in this call. |

## Markdown tools

### `markdown_list`

List converted markdown documents with file sizes and page counts. Sends progress notifications.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `limit` | integer | all | Maximum items to return. |
| `offset` | integer | `0` | Items to skip. |
| `embedded` | boolean | ignored | Applies to `catalog_list` only. |

### `markdown_read`

Read a converted markdown document, or one page of it.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Paper stem. |
| `page` | integer | whole document | Page number, 1-based. |

## Scribe tools

### `scribe_health`

Check the scribe server: model status, version, in-flight conversions and available VLM slots. Uses the first server in `scribe.servers`.

Changes state: no. Takes no parameters.

### `scribe_queue`

Queue a stored source document (PDF, HTML or EPUB) for conversion and return at once. Publishes `papers.ingested`, which the `hs scribe watch-events` daemon consumes; the conversion happens after the call returns. One document's conversion is bounded by the scribe timeout ceiling of 3600 s, so it far outlives a tool call. Poll `catalog_list`, which reports `converted` and `conversion_failed` per stem, or call `distill_index` once the markdown exists. Use `scribe_convert` only when the markdown is needed inside the call. Refuses a stem with no stored source, so a queue entry cannot point at nothing. Needs the event bus and a running watcher.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Stem of a PDF, HTML or EPUB source stored in the papers directory. |

### `scribe_convert`

Blocks until the conversion finishes; use `scribe_queue` instead unless the markdown is needed inside this call. A conversion is bounded by the scribe timeout ceiling of 3600 s. Converts a stored source document (PDF, HTML or EPUB; PDF is preferred when several exist) to markdown through the code path the scribe event watcher uses. PDFs go to the scribe server; HTML and EPUB are parsed locally. Writes the markdown to storage, stamps the catalog and announces `scribe.completed`. Markdown that already exists is not converted again; it is re-announced (`hs scribe reconvert` forces a fresh conversion). Fails, naming the cause, when storage is unreachable, the source is missing, the document is refused (not a PDF, a paywall page, empty or looping output) or the catalog stamp did not land. While a conversion runs, the tool sends a progress notification about every 20 seconds. Use `markdown_read` to fetch the content. Needs the event bus.

When a PDF's markdown is stored, and when a PDF is found already converted without word boxes, the tool also publishes `scribe.boxes`; the scribe watcher builds the word boxes after the call returns.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Stem of a PDF, HTML or EPUB source stored in the papers directory. |

## Distill tools

### `distill_search`

Semantic search across indexed documents. Returns ranked hits with text snippet, metadata, score, page, line range and `links` (when links are configured). See [Search](../explanation/search.md) for how hits are ranked.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `query` | string | required | Search query. |
| `limit` | integer | `10` | Maximum results. |
| `year` | string | none | Year filter, for example `>2020`, `2023`, `>=2021`. |
| `include_text` | boolean | `true` | When `false`, omit `chunk_text` from each hit: a metadata-only response for ranking or de-duplicating large result sets. Score and ranking are unaffected. |

### `abstract_search`

Semantic search over paper abstracts: the `abstracts` store, one record per paper. Each record is the paper's title and abstract; the abstract comes from the local OpenAlex catalog, else the paper's catalog abstract, else the converted markdown's `## Abstract` section. Papers with no usable abstract are not indexed. Body sections are excluded, so it is more precise than `distill_search` for finding the paper that argues a claim. Returns ranked hits with score, title, DOI, year and abstract text.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `query` | string | required | Search query. |
| `limit` | integer | `10` | Maximum results. |
| `year` | string | none | Year filter, as for `distill_search`. |
| `include_text` | boolean | `true` | When `false`, omit the abstract text from each hit. |

### `distill_status`

Distill system status: the `academic` store's document and chunk counts, compute device and server version.

Changes state: no. Takes no parameters.

### `distill_exists`

Check whether a document has been indexed in the vector database.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Stem of the indexed document to check. |

### `distill_index`

Index a converted markdown document into the vector database.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Stem of a markdown document to index. |

### `distill_reconcile`

Report Qdrant `doc_id`s whose markdown object is missing. Report only: a `dry_run` of `false` is ignored and the response says so with `requested_apply_ignored: true` (the field is `false` when the caller did not ask to apply). The write path is `hs distill reconcile --reembed` or `hs distill purge <doc_id>`. Sends progress notifications.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `dry_run` | boolean | `true` | Ignored; the tool only reports. |
| `limit` | integer | every `doc_id` | Maximum number of `doc_id`s to check for missing markdown. |

### `distill_scan_repetitions`

Scan markdown objects for VLM repetition artifacts and report the `doc_id`s whose cleanup truncation count exceeds `threshold`. Report only; remediate with `hs distill purge <doc_id>` or a re-index. An unreadable or non-UTF-8 object is an error, never skipped. Sends progress notifications.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `limit` | integer | `100000` | Maximum number of markdown objects to scan. |
| `threshold` | integer | `20` | Flag documents whose repetition truncation count exceeds this value. Lower is more aggressive. |

### `distill_reindex`

Re-index a document from storage with fresh catalog metadata, replacing its vectors in place. The new chunks are upserted and stale tail chunks are removed only after indexing succeeds, so a failure leaves the previous vectors untouched.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Stem of the document to re-index. |

### `distill_backfill`

Find catalog entries that are converted but not embedded and retry indexing for each. Documents previously stamped with an `embedding_skip` reason are left out unless `retry_skipped` is `true`. Sends progress notifications.

Changes state: yes (unless `dry_run` is `true`).

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `dry_run` | boolean | `true` | Report what would be re-indexed without writing. |
| `limit` | integer | none | Maximum number of documents to attempt in this call. |
| `retry_skipped` | boolean | `false` | Also retry documents stamped with an `embedding_skip` reason. |

## Personal tools

These tools work on the personal-documents store. See [Manage personal documents](../how-to/manage-personal-documents.md).

### `personal_search`

Semantic search over the personal documents: the `personal` store.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `query` | string | required | Search query. |
| `limit` | integer | `10` | Maximum results. |
| `category` | string | all categories | Restrict to one category: `medical`, `financial`, `education`, `legal`, `employment`, `tax`, `insurance`, `correspondence` or `other`. |

### `personal_list`

List ingested personal documents, most recent first.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `limit` | integer | `50` | Maximum entries to return. |
| `category` | string | all categories | Restrict to one category. |

### `personal_read`

Read the converted markdown of one personal document.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Document stem. |

### `personal_add`

Ingest a file already staged in the personal inbox. Takes a file name, not a path: absolute paths, path separators and a leading dot are rejected. The file must already be in the configured inbox directory. The tool runs the same ingest pipeline as `hs personal add`, blocks until indexing finishes (a long PDF can take many minutes) and reports the assigned stem, title and category. Sends progress notifications.

For a PDF the tool also publishes `scribe.boxes` once the document is stored and indexed, and the scribe watcher builds its word boxes after the call returns. A PDF is refused before it is converted when the broker cannot be reached.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `filename` | string | required | File name inside the personal inbox. |
| `category` | string | model-chosen | Override the category. Must be one of the configured categories. |
| `title` | string | model-chosen | Override the title. |
| `force` | boolean | `false` | Replace an existing document with the same stem or the same file content. |

### `personal_reindex`

Re-chunk and re-embed an already-ingested personal document from its stored markdown. Vectors are replaced in place; nothing is deleted first, so a failed re-embed leaves the previous vectors untouched.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `stem` | stem | required | Document stem to re-chunk and re-embed. |

## Status tools

### `system_status`

Full pipeline status: PDF count, markdown count, catalog count, embedded-document count and the health of every service. The `history` pane matches `catalog_recent`'s defaults. The `hs status` command renders this data; see [Status dashboard](status-dashboard.md).

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `include_repaired` | boolean | `false` | Include rows stamped by a `catalog_repair` backfill in `history`, annotated `"repair": true`. |

## OpenAlex tools

These tools query the local OpenAlex database and make no external calls. They are listed only when the OpenAlex corpus is ready; see [Tool visibility](#tool-visibility).

### `openalex_search`

Search the local OpenAlex catalog by free text (BM25 over title and abstract). Returns a JSON array of works with `openalex_id`, DOI, title, year, citations and BM25 score. Needs the full-text index from `hs openalex build-fts`.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `query` | string | required | Free-text query. |
| `max_results` | integer | `10` | Maximum results; maximum `200`. |
| `year_from` | integer | none | Minimum publication year. |
| `year_to` | integer | none | Maximum publication year. |
| `min_citations` | integer | none | Minimum `cited_by_count`. |
| `sort` | string | `relevance` | `relevance`, `citations` or `year`. |

### `openalex_get`

Look up one work by OpenAlex ID or DOI. Returns JSON with the work and its denormalised authors and topics.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `id_or_doi` | string | required | OpenAlex work ID (for example `W2741809807`) or DOI (for example `10.1234/x`). |

### `openalex_references`

List the works a given work cites (outbound references): `openalex_id`, DOI, title, year and `cited_by_count`.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `openalex_id` | string | required | OpenAlex work ID whose references to list. |
| `limit` | integer | `100` | Maximum results; maximum `1000`. |

### `openalex_citations`

List the works that cite a given work (forward citation chaining).

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `openalex_id` | string | required | OpenAlex work ID to find citing works for. |
| `limit` | integer | `100` | Maximum results; maximum `1000`. |
| `year_from` | integer | none | Minimum publication year. |
| `sort` | string | `citations` | `citations` or `year`. |

### `openalex_authors_by_topic`

Top authors for an OpenAlex topic, ranked by total `cited_by_count`. Returns a JSON array of `openalex_id`, `display_name`, `cited_by_count` and `papers_in_topic`.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `topic_id` | string | required | OpenAlex topic ID, for example `T13975`; take it from `openalex_search` or the `topics` table. |
| `limit` | integer | `25` | Maximum authors; maximum `200`. |

## Resources

| URI template | MIME type | Content |
|---|---|---|
| `catalog:///{stem}` | `application/yaml` | The paper's catalog entry: metadata, conversion info and page offsets. |
| `markdown:///{stem}` | `text/markdown` | The full converted markdown. |
| `markdown:///{stem}/page/{page}` | `text/markdown` | One page of the converted markdown; `page` is 1-based. |

`resources/list` enumerates one `catalog:///` and one `markdown:///` resource for every catalog entry and converted document. An invalid stem is an invalid-parameters error; a missing entry is resource-not-found; a page outside the document is an invalid-parameters error naming the page count.

## Configuration it reads

The server reads `~/.home-still/config.yaml` once at startup and `~/.home-still/secrets.env` before that. Keys are defined in [Configuration](configuration.md).

| Section | Required | Used for |
|---|---|---|
| `storage:` | yes | Catalog, papers and markdown objects. |
| `events:` | yes | `papers.ingested`, `scribe.completed` and `scribe.boxes` announcements. |
| `scribe:` (`servers`, `convert_timeout_secs`, `timeout_policy`, `epub`) | no | `scribe_health`, `scribe_convert`. |
| `distill:` (`servers`) | no | Distill and personal search and indexing tools. |
| `paper:` | no | Provider settings for `paper_*` tools. |
| `docs:` (`lan_base_url`, `remote_base_url`, `link_ttl_secs`, `default_subject`) | no | Signed links in search hits and `cite`. Links are off, and `cite` fails, until both base URLs are set. |
| `storage:` | yes | Short-link codes (`cite` with `short`) are records beside the documents. |
| `openalex:` (`db_path`) | no | The local OpenAlex database for the `openalex_*` tools. |

| Variable | Used for |
|---|---|
| `HS_BACKEND_TOKEN` | Bearer token required by the HTTP transport. |
