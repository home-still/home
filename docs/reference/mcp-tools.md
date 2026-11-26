# MCP tools reference

`hs-mcp` is the Model Context Protocol server for home-still. It has ten tools and three prompts: this page lists them, with the settings the server reads. To connect a client, see [Connect Claude and agents](../how-to/connect-claude-and-agents.md).

`hs-mcp` does not search. Searching the corpus is the `recall` tool of the `myelin` memory server, which a client connects beside this one. A hit from `recall` carries `store` and `meta.doc_id`; those two values are what `read`, `cite` and `status` take.

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

### HTTP routes

| Route | Purpose |
|---|---|
| `/mcp` | The MCP endpoint. |
| `GET /mcp/status` | The dashboard snapshot that [`hs status`](status-dashboard.md) draws, as JSON. `?include_repaired=true` also lists failures that were repaired since. It is not a tool: an agent asks [`status`](#status). |

### Startup requirements

| Requirement | Behaviour when absent |
|---|---|
| `storage:` section in `~/.home-still/config.yaml` | The server does not start. |
| `events:` section in `~/.home-still/config.yaml` | The server does not start. The broker connection is made on the first publish, so a broker outage affects only `acquire`, `capture`, `synthesis_save` and `personal_add`; each refuses before it stores anything. |
| A malformed config section | The server does not start; the error names the section. |
| `scribe.servers`, `distill.servers` | The server starts. `status` reports the part it could not ask as an `error`. |
| The local OpenAlex corpus | The server starts. `discover` and `graph` with `source: "openalex"` fail with the reason and the command that builds the corpus. The check runs once at startup, so restart the server after building it. See [Use the OpenAlex catalog](../how-to/use-the-openalex-catalog.md). |

### What the tools cannot do

Maintenance and destructive operations are CLI-only. Deleting a personal document is `hs personal delete`; purging records is `hs distill purge` and the `hs pipeline purge-*` commands; repairs and re-indexing are listed under [Tools removed in favour of the CLI](#tools-removed-in-favour-of-the-cli). See [CLI reference](cli.md).

The server exposes no MCP resources. `read` returns a document's card and text, and `documents` lists what a store holds.

### Prompts

| Prompt | Arguments | Purpose |
|---|---|---|
| `guidance` | `question` | Answer a question from the corpus: profile, recall, cite each claim, answer with links. |
| `verify` | `claim` | Check a claim against the corpus: find support and contradiction, cite both, give a verdict. |
| `brief` | `topic` | Write a cited brief on a topic, widen the corpus if it is thin, and save the brief. |

## Conventions

| Term | Meaning |
|---|---|
| store | One of `academic` (papers), `references` and `abstracts` (derived from the papers), `personal` (the user's own records), `web` (captured pages), `syntheses` (saved answers) and `memory`. `memory` holds no documents. A tool given `references` or `abstracts` works on the paper the hit belongs to. |
| doc_id | A document's id within its store: `meta.doc_id` of a search hit, or `doc_id` of a `documents` row. An id that is empty, absolute or contains a path separator or `..` is rejected with an invalid-parameters error before the tool runs. |
| optional | The parameter may be omitted; the table gives the value used then. |

A tool that fails returns an MCP tool error whose text says what failed and, where there is one, what to do instead. A storage outage is an error; it is never reported as "no such document" or as an empty list.

"Changes state: yes" means the tool's `read_only_hint` is `false`: it writes to storage and announces on the event bus.

## Tools

| Tool | Purpose | Changes state |
|---|---|---|
| [`cite`](#cite) | Check a quote against its document and return a verifiable citation. | only with `short` |
| [`read`](#read) | A document's card and a slice of its text. | no |
| [`documents`](#documents) | List the documents of one store. | no |
| [`discover`](#discover) | Look outside the corpus for works worth adding. | no |
| [`graph`](#graph) | Step along the citation graph from a DOI. | no |
| [`acquire`](#acquire) | Download a paper by DOI into `academic`. | yes |
| [`capture`](#capture) | Keep a web page in `web`. | yes |
| [`personal_add`](#personal_add) | Ingest a file staged in the personal inbox. | yes |
| [`synthesis_save`](#synthesis_save) | Save an answer, with its verified citations, in `syntheses`. | yes |
| [`status`](#status) | How the pipeline is, or how one document is. | no |

### `cite`

Check a quote against the document it is from and return a citation a reader can verify. The quote is matched against the document's own text: markup, line breaks, spacing, hyphenation, case and typography may differ, the words may not. **A quote the document does not contain is a tool error** that names the closest passage, its lines and the share of the quote's words it holds; no citation is returned for it.

For a quote the document contains, the result is JSON:

| Field | Meaning |
|---|---|
| `store`, `doc_id` | What was asked. |
| `title`, `authors`, `year`, `doi`, `source_url` | From the document's catalog entry: `null`, or `[]` for `authors`, where the entry has none. |
| `quote` | `text` (the passage as the document has it), `exact` (`false` when the quote was found as the same words in the same order rather than as typed), `found_in` (`document` or `abstract`), `page`, `line_start` and `line_end`. `page` is the page the passage starts on: read from the word boxes when `boxes.status` is `located`, otherwise from the page offsets the conversion recorded for a PDF, otherwise `null`. The lines are lines of the converted text, and `null` for an abstract. |
| `boxes` | `status` is `located`, `not_located` (the word boxes are built and the passage could not be placed in them), `pending` (a PDF whose word boxes are not built yet) or `not_applicable` (the source is not a PDF, or the quote is from an abstract). `not_located` says the converted text and the words read from the PDF's pages disagree at the passage, so the quote is not confirmed on the page: in 100 sentences sampled from 20 converted papers (2026-11-25), all 5 that came back `not_located` were text the conversion had made up. `located` also carries `origin` (`text_layer`, or `ocr` when a page the passage is on was read by character recognition), `matched_ratio` (the share of the passage's words found on the page, in order) and `pages`: for each page the passage is on, the page number, its size in points and the rectangles around the passage's words. |
| `links` | `source` (the original document, at the passage's page when it is known) and `markdown` (the converted text, the passage selected), each a `lan` and a `remote` URL. When `boxes.status` is `located`, also `viewer` (the whole PDF, opened on the boxed passage) and `page` (that page as a picture, the passage boxed). `original_url` is `source_url` with a text fragment that selects the passage, or `null`. `short` is `lan`, `remote` and `code` when `short` was asked for, otherwise `null`. `expires_at` is when the links stop working. |

With `image` the result has a second content item: the page the passage starts on as a PNG, the passage boxed. The MCP server fetches it from the docs service at `docs.lan_base_url`.

For `references` the quote is checked against the document the reference list belongs to, and for `abstracts` against the abstract in the document's catalog entry. Links are signed for the store that holds the document. The call fails when `docs.lan_base_url` or `docs.remote_base_url` is not set, when the signing key is unavailable, when the store or the document is unknown, when the document has no converted text yet, and for `memory`. See [Open documents with links](../how-to/open-documents-with-links.md).

Changes state: only with `short`, which stores the code in the corpus.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `store` | string | `academic` | The store the passage was found in: the `store` of the hit. |
| `doc_id` | doc_id | required | The document's id: `meta.doc_id` of the hit. |
| `quote` | string | required | The passage, word for word: at least 4 words, at most 2000 characters. |
| `near_line` | integer | none | A line of the document near the passage. When the document says the quote more than once, the occurrence nearest this line is the one cited. Refused for `abstracts`. |
| `image` | boolean | `false` | Also return the page as an image with the passage boxed. The call fails unless `boxes.status` is `located`. |
| `short` | string | none | `viewer`, `page`, `source` or `markdown`: also store a 12-character code that redirects to that link, returned as `<base>/s/<code>` under `links.short`. The docs service answers the code with a redirect to the signed link, carrying its fragment, so the code expires with the link and is pruned by `hs docs prune`. `viewer` and `page` are refused unless `boxes.status` is `located`. Storing the code writes one small record to the corpus. |

### `read`

Read a document: its catalog card and a slice of its text. The result has two content items. The first is JSON:

| Field | Meaning |
|---|---|
| `store`, `doc_id` | What was asked. |
| `card` | `title`, `authors`, `year`, `doi`, `source_url`, `abstract`, `category`, `added` and `pages`, from the document's catalog entry. For `personal` the card is the one stored beside the document. |
| `converted` | Whether the document has text yet. When `false` there is no `slice` and no second item; ask [`status`](#status). |
| `slice` | What was returned: `line_start`, `line_end`, `page` (the page `line_start` is on), `total_lines`, `total_pages`, `cut_mid_line` and `next_line`. `next_line` is the line to go on from when the selection did not fit in `max_chars`, otherwise `null`. |
| `links` | Signed links that open the source and the converted text, when links are configured; otherwise `null`. |

The second item is the text, every line behind its number: `12: The delta grows`. The numbers are the ones `lines`, a hit's `meta.line_start` and a citation's `quote.line_start` count in.

The call fails when the store holds no document with that id (the error points at `documents`), when `page` and `lines` are both given, and when the selection is outside the document; the last error names the document's line or page count.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `store` | string | `academic` | The store the document is in. |
| `doc_id` | doc_id | required | The document's id. |
| `page` | integer | none | Read this page of the source, 1-based. Not together with `lines`. |
| `lines` | `[first, last]` | none | Read these lines of the converted text, 1-based and inclusive. Not together with `page`. With neither, reading starts at line 1. |
| `max_chars` | integer | `12000` | The most characters of text to return. The slice ends on a whole line. |

### `documents`

List the documents of one store, newest first. This lists what the corpus holds; to find documents by what they say, use `recall` on the `myelin` server.

The result is JSON: `store`, `matching` (how many documents pass the filters), `documents` and `next_cursor`. Each row has `doc_id`, `title`, `authors`, `year`, `doi`, `source_url`, `category` (when the entry has one), `added`, `converted` and `indexed`. `next_cursor` is `null` on the last page.

The call fails for a store that holds no documents of its own (`references`, `abstracts`, `memory`), for an unknown store, for an `added_after` that is not a timestamp or a date, and for a cursor that does not belong to the listing.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `store` | string | `academic` | `academic`, `personal`, `web` or `syntheses`. |
| `title_contains` | string | none | Only documents whose title contains this text, whatever its case. |
| `added_after` | string | none | Only documents added after this moment: an RFC 3339 timestamp, or a date read as the start of that day, UTC. |
| `limit` | integer | `20` | Documents per page, at most 200. |
| `cursor` | string | none | `next_cursor` of the previous page, with the same filters. |

### `discover`

Look outside the corpus for works worth adding to it. `source` chooses where: `providers` asks the online paper providers (arXiv, OpenAlex, Semantic Scholar, Europe PMC, CrossRef and, with an API key, CORE; see [Paper providers](paper-providers.md)); `openalex` reads the local OpenAlex corpus without the network.

Give exactly one of `query` and `doi`.

| Request | Result |
|---|---|
| `source: "providers"`, `query` | `{source, works, provider_failures}`. `provider_failures` names each provider that did not answer; the call fails only when none did. |
| `source: "providers"`, `doi` | `{source, work}`: one work's metadata. |
| `source: "openalex"`, `query` | `{source, works}`: works that contain every term of the query, best match first. |
| `source: "openalex"`, `doi` | `{source, work}`: the work with its `authors` and `topics`. |
| `source: "openalex"`, `query`, `kind: "authors"` | `{source, topic, authors}`: the most cited authors of the topic `query` names, by its id (`T10001`) or its exact name. |

The call fails when both or neither of `query` and `doi` are given, when `limit` is out of range, when `kind: "authors"` is asked of `providers` or with a `doi`, when a year filter is given to anything but a search for works, when the DOI is not in the source, and when the local corpus is not built.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `source` | string | required | `providers` or `openalex`. |
| `query` | string | one of `query`, `doi` | What to search for. |
| `doi` | string | one of `query`, `doi` | The DOI of one work to look up. |
| `kind` | string | `works` | `works`, or `authors` (with `source: "openalex"` and a `query` naming a topic). |
| `year_from` | integer | none | Only works published in this year or later. |
| `year_to` | integer | none | Only works published in this year or earlier. |
| `limit` | integer | `10` | How many results a search returns, 1 to 200. |

### `graph`

Step along the citation graph from a paper's DOI. `direction` is `references` (the works the paper cites) or `citations` (the works that cite it). `source` is `semantic_scholar` (the Graph API, online; see [Paper providers](paper-providers.md)) or `openalex` (the local OpenAlex corpus).

The result is JSON: `doi`, `direction`, `source`, `works`, `returned` and `truncated`. `truncated` says more works matched than were returned. The works carry their source's fields: Semantic Scholar gives DOI, title, year, authors, venue and citation count; OpenAlex gives its id, DOI, title, year and citation count. A work without a year sorts last under `sort: "year"` and is dropped by `year_from`.

The call fails when the DOI is malformed, when `limit` is out of range, when the source has no work with that DOI, and, for `openalex`, when the local corpus is not built.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | The DOI of the paper to step from. |
| `direction` | string | required | `references` or `citations`. |
| `source` | string | required | `semantic_scholar` or `openalex`. |
| `limit` | integer | `100` | How many works to return, 1 to 1000. |
| `year_from` | integer | none | Only works published in this year or later. |
| `sort` | string | `year` | `year` (newest first) or `citations` (most cited first). |

### `acquire`

Download a paper by DOI into the `academic` store. The tool tries arXiv, MDPI and the providers' open-access locations, stores the file, writes its catalog entry and announces it. Conversion to text, indexing and word boxes then run in the background.

The result is JSON: `store`, `doc_id`, `doi`, `state` and `converted`. `state` is `queued` (stored by this call; the answer also carries the file's size and SHA-256) or `present` (the store already held it; nothing was downloaded). When `converted` is `false`, ask [`status`](#status) with the `doc_id` until it is, then `read` and `cite`.

The call fails when no open copy can be downloaded, when the metadata lookup fails, when the catalog entry cannot be written, and when the event bus is unreachable, in which case nothing is downloaded. Calling again repairs a half-finished acquisition.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `doi` | string | required | The paper's DOI, for example `10.1038/s41586-021-03819-2`. |

### `capture`

Keep a web page in the `web` store, so it can be searched, read and cited like a paper. The caller fetches the page and passes its content; the server does not fetch.

The page's `doc_id` is made from its address without the fragment, so **capturing the same address again replaces the page captured before**, together with the text converted from it. The tool is annotated as destructive for that reason.

The result is JSON: `store`, `doc_id`, `source_url` (the address without its fragment), `size_bytes`, `replaced` and `next`. Conversion and indexing run in the background; [`status`](#status) with the `doc_id` says when the page can be read and cited.

The call fails for content over 2 MiB, for empty content, for an address that is not `http` or `https`, for HTML that is a paywall or login page rather than the article, and when the event bus is unreachable. A refused call stores nothing.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `url` | string | required | The page's address. |
| `content` | string | required | The whole page, at most 2 MiB. Citations are checked against this text, so not a summary. |
| `content_type` | string | required | What `content` is written in: `html`, `markdown` or `text`. |
| `title` | string | none | The page's title, as `documents` and citations show it. |

### `personal_add`

Ingest a file already staged in the personal inbox. The user puts the file in the inbox directory; the tool takes its file name, runs the same pipeline as `hs personal add` and blocks until the document is indexed. Converting a long PDF can take many minutes.

The result is JSON: `store`, `doc_id`, `title`, `category`, `chunks_indexed` and `source_file`.

The call fails for a name that has a path separator or a leading dot, for a file that is not in the inbox, for a category that is not configured, and for a document the store already holds unless `force` is set. See [Manage personal documents](../how-to/manage-personal-documents.md).

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `filename` | string | required | The file's name inside the personal inbox. Never a path. |
| `category` | string | chosen by the model | One of the configured categories. |
| `title` | string | chosen by the model | The document's title. |
| `force` | boolean | `false` | Replace an existing document with the same id or the same content. |

### `synthesis_save`

Save an answer as a document in the `syntheses` store, so it can be found, read and cited later. Every citation is checked exactly as `cite` checks it. **When one document does not contain its quote, nothing is saved** and the error names the citation by its position.

The saved document is markdown that starts with front matter: the title, the creation time, the tags and each verified citation (`store`, `doc_id`, `page` and `lines` where they are known, and `quote` as the document has it). Each call saves a new document.

The result is JSON: `store`, `doc_id`, `citations_verified` and `next`. Indexing runs in the background.

The call fails for an empty title or body, for a body over 1 MiB, for an empty `citations` list, for a citation that fails the check, and when the event bus is unreachable.

Changes state: yes.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `title` | string | required | The question the synthesis answers, or its subject. |
| `body` | string | required | The synthesis, as markdown, at most 1 MiB. |
| `citations` | list | required, at least one | Each item is `{store, doc_id, quote}`: the store and document of the hit, and the passage word for word. |
| `tags` | list of strings | none | Words to file the synthesis under. |

### `status`

How the pipeline is, or how one document is. One part that cannot answer does not fail the call: that part carries `{"error": "<reason>"}` in place of its value and the rest of the answer stands.

Without `doc_id` the result is JSON:

| Field | Meaning |
|---|---|
| `queues` | One row per queue: sources waiting to be converted, converted documents waiting to be indexed, and PDFs waiting for their word boxes. Each has `queue`, `holds` and `depth` (`waiting` and `in_progress`). |
| `index` | What the index holds for `store`: `store`, `documents`, `documents_is_a_floor`, `records` and `embed_model`. |
| `converters` | One row per conversion server: `server`, `ready`, `slots_total`, `slots_available`, `converting` and `backend_status`. |

This form reads only sources that answer at once and never lists the storage.

With `doc_id` the result is JSON:

| Field | Meaning |
|---|---|
| `store`, `doc_id` | What was asked. |
| `source` | `stored`, and `extension`: the format the source is stored in. |
| `catalog_entry` | Whether the document has a catalog entry. |
| `converted` | Whether the document has text. |
| `records` | For the document's own store and each store derived from it, how many records the index holds for the document. |
| `boxes` | `built`, `pending` or `not_applicable` (the source is not a PDF). |
| `failure` | `null`, or the failure recorded for the document: `stage` (`conversion` or `indexing`), `reason` and `at`. |

A document nothing is known about is an answer, not an error: its source is not stored and it is not converted.

Changes state: no.

| Parameter | Type | Required or default | Meaning |
|---|---|---|---|
| `store` | string | `academic` | The store to report on. With `doc_id`, the store the document is in. |
| `doc_id` | doc_id | none | Report on this one document instead of the pipeline. |

## Tools removed in favour of the CLI

Earlier releases exposed about forty tools. The ones below are gone; this table says where each one went.

| Removed tool | Use instead |
|---|---|
| `distill_search`, `abstract_search`, `personal_search` | `recall` on the `myelin` server, with `stores` naming the store. |
| `paper_search`, `paper_get`, `openalex_search`, `openalex_get`, `openalex_authors_by_topic` | [`discover`](#discover) |
| `paper_references`, `paper_citations`, `openalex_references`, `openalex_citations` | [`graph`](#graph) |
| `paper_download` | [`acquire`](#acquire) |
| `catalog_list`, `catalog_recent`, `markdown_list`, `personal_list` | [`documents`](#documents) |
| `catalog_read`, `markdown_read`, `personal_read`, and the `catalog:///` and `markdown:///` resources | [`read`](#read) |
| `scribe_health`, `distill_status`, `distill_exists` | [`status`](#status) |
| `system_status` | `hs status`, which reads `GET /mcp/status`; an agent asks [`status`](#status). |
| `scribe_convert` | `hs scribe convert` |
| `scribe_queue` | `hs scribe queue <STEM>` |
| `distill_index` | `hs distill index` |
| `distill_reconcile` | `hs distill reconcile` |
| `distill_reindex` | `hs distill reindex` |
| `distill_backfill` | `hs distill backfill` |
| `distill_scan_repetitions` | `hs distill scan-repetitions` |
| `personal_reindex` | `hs personal reindex` |
| `catalog_repair` | `hs pipeline repair-catalog` |
| `catalog_backfill_title` | `hs pipeline backfill-titles` |
| `dedupe_url_encoded` | `hs migrate dedupe-url-encoded` |

See [CLI reference](cli.md) for each command.

## Configuration it reads

The server reads `~/.home-still/config.yaml` once at startup and `~/.home-still/secrets.env` before that. Keys are defined in [Configuration](configuration.md).

| Section | Required | Used for |
|---|---|---|
| `storage:` | yes | Sources, converted text, catalog entries, word boxes and short-link codes. |
| `events:` | yes | Announcing what `acquire`, `capture`, `synthesis_save` and `personal_add` stored; the queue depths in `status`. |
| `scribe:` (`servers`) | no | Converter readiness in `status`. |
| `distill:` (`servers`) | no | Record counts in `status`; indexing in `personal_add`. |
| `paper:` | no | Provider settings for `discover`, `graph` and `acquire`. |
| `docs:` (`lan_base_url`, `remote_base_url`, `link_ttl_secs`, `default_subject`) | no | Signed links in `read` and `cite`. Links are off, and `cite` fails, until both base URLs are set. |
| `openalex:` (`db_path`) | no | The local OpenAlex corpus for `discover` and `graph`. |

| Variable | Used for |
|---|---|
| `HS_BACKEND_TOKEN` | Bearer token required by the HTTP transport. |
