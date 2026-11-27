# `hs` command reference

`hs` is the home-still command-line tool. This page lists every command with its arguments, its flags and one example. Each flag table matches the output of `hs <command> --help`. The configuration the commands read is described in [configuration](configuration.md); the services they start are described in [services and ports](services-and-ports.md).

| Command | Description |
|---|---|
| [`hs scribe`](#hs-scribe) | Convert documents to markdown through scribe servers and run the event-driven conversion pipeline. |
| [`hs paper`](#hs-paper) | Academic paper search, lookup, and download. |
| [`hs personal`](#hs-personal) | Personal documents: ingest medical/financial/etc records into a private store. |
| [`hs distill`](#hs-distill) | Distill markdown into vector embeddings for semantic search. |
| [`hs docs`](#hs-docs) | Signed, expiring links to the documents in the corpus. |
| [`hs status`](#hs-status) | Live dashboard — pipeline health, services, recent activity. |
| [`hs restart`](#hs-restart) | Restart all running services (system services, daemons, containers). |
| [`hs upgrade`](#hs-upgrade) | Check for a newer release and upgrade `hs`, its companion binaries and the managed services. |
| [`hs serve`](#hs-serve) | Run a home-still service on this machine: in the foreground, as an installed service, or in the background. |
| [`hs cloud`](#hs-cloud) | Remote cloud access — enrollment, gateway management. |
| [`hs mcp`](#hs-mcp) | Install or remove the home-still MCP server entry in Claude Desktop, Claude Code and OpenCode. |
| [`hs config`](#hs-config) | View and manage configuration. |
| [`hs migrate`](#hs-migrate) | Run data migrations. |
| [`hs pipeline`](#hs-pipeline) | Cross-service pipeline operations (rebuild from papers, etc.). |
| [`hs openalex`](#hs-openalex) | Local OpenAlex catalog (DuckDB) — bulk-load entities, query, build FTS. |

## Global flags

Every command accepts these flags, before or after the subcommand.

| Flag | Default | Meaning |
|---|---|---|
| `--color <COLOR>` | `auto` | Color output: `auto`, `always` or `never`. `auto` colors only when standard error is a terminal; `FORCE_COLOR` forces color, and `NO_COLOR` or `TERM=dumb` turns it off. |
| `--output <OUTPUT>` | `text` | Output format: `text`, `json` or `ndjson`. `hs status`, `hs distill search`, `hs config show` and the `hs paper` commands honor it; other commands print text. `json` and `ndjson` also turn off color and progress display. |
| `--quiet` | off | Suppress all output except results. |
| `--verbose` | off | Show debug-level output. |
| `-y`, `--yes` | off | Skip interactive prompts and assume yes. |

A few commands define their own flag with the same name, and the local flag replaces the global one on that command: `--yes` on `hs pipeline rebuild`, `hs pipeline purge-skipped`, `hs pipeline purge-poisoned`, `hs pipeline reap-phantoms` and `hs pipeline reconvert-failed` (long form only, no `-y`), and `--verbose` on `hs distill diagnose`.

Secrets in `~/.home-still/secrets.env` are exported into the environment before any command runs; a secrets file that cannot be read stops the command with exit code 2.

## Exit codes and interruption

| Code | Meaning |
|---|---|
| 0 | The command finished its job. |
| 1 | The command failed. Batch commands report each failed item, then fail as a whole when any item failed. |
| 2 | Invalid arguments, an unreadable `secrets.env`, or, for `hs paper`, invalid input or a paper with no download URL. |
| 3 | `hs paper` only: the network or every provider failed (HTTP error, rate limit, provider unavailable, circuit breaker open). |
| 130 | A second Ctrl+C. |
| 143 | A second SIGTERM, on commands that stop gracefully. |

`hs scribe watch-events` and `hs distill watch-events` exit with an error when the event stream ends, so a service manager restarts them.

These commands stop gracefully on the first Ctrl+C: the item in progress finishes, a summary of what was and was not done is printed, and the command exits non-zero. A second Ctrl+C exits immediately.

- `hs pipeline catch-up`, `hs pipeline rebuild`, `hs pipeline purge-skipped`, `hs pipeline purge-poisoned`, `hs pipeline reap-phantoms` and `hs pipeline reconvert-failed`
- `hs migrate sharding`, `hs migrate move-root-orphans`, `hs migrate personal-to-storage`, `hs migrate quarantine-bad-content` and `hs migrate canonicalize-doi-stems`
- `hs distill abstracts build` and `hs distill abstracts reconcile`
- `hs scribe inbox run`, `hs scribe inbox sweep`, `hs scribe boxes catch-up`, `hs serve mcp`, `hs serve docs` and `hs status`

For these commands SIGTERM is also a graceful stop, so `systemctl stop` ends a service cleanly. A command that does not stop within 30 seconds of the first request is cancelled and exits with `interrupted`. Every other command is cancelled at once by the first Ctrl+C and exits with an error.

## `hs scribe`

Convert documents to markdown through scribe servers and run the event-driven conversion pipeline.

```text
hs scribe [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs scribe convert` | Convert a PDF to markdown (sends to scribe server). |
| `hs scribe watch-events` | Consume `papers.ingested` and `scribe.boxes` events from the event bus: convert each ingested document through the scribe servers and write the markdown back to storage, and build and store the word boxes of each PDF that is asked for. |
| `hs scribe inbox` | Sweep `papers/manually_downloaded/` in storage, move each file to its place under `papers/`, and publish `papers.ingested` so conversion starts. |
| `hs scribe catalog-backfill` | Create catalog entries for markdown files in `scribe.output_dir` that have none. |
| `hs scribe reconvert` | Clear a document's conversion stamps and derived artifacts, then republish `papers.ingested` so the watcher converts it again. |
| `hs scribe boxes` | Word-box sidecars: where every word of a PDF sits on its page, which is what lets a quotation be highlighted in the document it is from. |
| `hs scribe queue` | Announce that one stored source needs converting: publish `papers.ingested` so the scribe watcher converts it. |

The conversion commands read the `scribe:` section; the watchers and `hs scribe boxes` also need `storage:` and `events:`. See [configuration](configuration.md).

Example:

```bash
hs scribe --help
```

### `hs scribe convert`

Convert a PDF to markdown (sends to scribe server).

```text
hs scribe convert [OPTIONS] <INPUT>
```

| Name | Default | Meaning |
|---|---|---|
| `<INPUT>` | required | Path of the PDF to convert. |
| `--out <OUT_FILE>` | none | File to write the markdown to. Without it the markdown goes to `scribe.output_dir/<shard>/<stem>.md` when `scribe.output_dir` is set to a directory, otherwise to standard output. |
| `--server <SERVER>` | none | Scribe server URL. Replaces `scribe.servers` for this run. |

Converts one PDF and exits. The server list is `--server`, else `scribe.servers`; with neither the command fails and names `scribe.servers`. With one server the command checks its health first and refuses to start when the server reports a status other than `ok`, printing the reason (for example, not enough free GPU memory). With several servers it picks among the reachable ones.

A server whose URL is a cloud gateway needs the credentials written by `hs cloud enroll`.

The command counts the PDF's pages, cleans repetition artifacts out of the result, and runs the quality check. A conversion that contains a repetition loop, or in which the server skipped regions, is rejected: the command exits non-zero and writes no output.

When `HS_SCRIBE_DIAG_DIR` is set, the command writes one JSONL diagnostics file per document into that directory.

Example:

```bash
hs scribe convert paper.pdf --out paper.md --server http://<gpu-host>:7433
```

### `hs scribe watch-events`

Consume `papers.ingested` and `scribe.boxes` events from the event bus: convert each ingested document through the scribe servers and write the markdown back to storage, and build and store the word boxes of each PDF that is asked for.

```text
hs scribe watch-events [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--server <SERVER>` | none | Scribe server URL. Replaces `scribe.servers` for this run and forms a single tier. |

Runs until stopped. It needs `storage:`, `events:` (a NATS backend) and `scribe.servers`, and it refuses to start on a host where libpdfium cannot be bound. It reads two queues side by side.

Conversions (`papers.ingested`): the source's key says which store it belongs to (`papers/` is the `academic` store's prefix, and each other store has its own), and the markdown and the catalog entry are stored in that store. A key under no store's prefix, and a file type the store does not take, are refused for good. A `.md` or `.txt` source is not sent to a server: its text is stored as the markdown, with its line endings made `\n`; it must be UTF-8 and at most 16 MiB. For the other types, servers are grouped into tiers by `backend`, in `scribe.servers` order; a paper goes to the first tier and falls through to the next when a backend-class failure occurs. Within a tier the least-loaded server takes the paper. A tier converts at most the sum of its servers' `concurrency` at once. When a PDF's markdown is stored, the watcher publishes `scribe.boxes` for it.

Word boxes (`scribe.boxes`): the watcher reads the PDF from storage, posts it to a scribe server's `/scribe/boxes`, checks that the sidecar it gets back was built from those bytes, and stores it under the store's `boxes/` prefix. The servers in `scribe.servers` take the requests in turn, whatever their tier, and as many requests run at once as there are servers; when a server cannot answer, the next is tried. A request for a key that is not where a store keeps a PDF, or for a PDF no server can read, is dropped with the reason logged. Any other failure is retried until `events.nats.max_deliver` deliveries are used up; `hs scribe boxes status` shows what is still missing and `hs scribe boxes catch-up` queues it again.

The command exits non-zero when the event stream ends. Run it as a service with `hs serve scribe-watch --install`, and the service manager restarts it.

Example:

```bash
hs scribe watch-events
```

### `hs scribe inbox`

Sweep `papers/manually_downloaded/` in storage, move each file to its place under `papers/`, and publish `papers.ingested` so conversion starts.

```text
hs scribe inbox [OPTIONS] [COMMAND]
```

| Command | Description |
|---|---|
| `hs scribe inbox run` | Run the inbox watcher in the foreground (Ctrl+C to exit). Default when no subcommand is given. |
| `hs scribe inbox sweep` | Do one sweep of the inbox prefix and exit. Useful for testing. |
| `hs scribe inbox install` | Install a user-level daemon that runs the inbox watcher at login. macOS: LaunchAgent plist. Linux: systemd --user unit. |
| `hs scribe inbox uninstall` | Remove the user-level daemon. |
| `hs scribe inbox status` | Report the daemon's running state. |

With no subcommand, `hs scribe inbox` behaves as `hs scribe inbox run`.

The inbox polls the storage prefix `papers/manually_downloaded/` every `scribe.inbox_poll_interval_secs` seconds (minimum 1). It accepts `.pdf`, `.html`, `.htm` and `.epub` files; an EPUB is unpacked to HTML. Files modified in the last 5 seconds are left for the next sweep, and other extensions are ignored. A file whose name cannot be a paper stem is moved to `corrupted/` once, with the reason logged. It needs `storage:` and `events:`.

Example:

```bash
hs scribe inbox
```

### `hs scribe inbox run`

Run the inbox watcher in the foreground (Ctrl+C to exit). Default when no subcommand is given.

```text
hs scribe inbox run [OPTIONS]
```

The command takes no arguments or flags of its own.

Polls until stopped. Ctrl+C or SIGTERM finishes the file in progress, prints a summary and exits.

Example:

```bash
hs scribe inbox run
```

### `hs scribe inbox sweep`

Do one sweep of the inbox prefix and exit. Useful for testing.

```text
hs scribe inbox sweep [OPTIONS]
```

The command takes no arguments or flags of its own.

Runs one sweep and exits. The command exits non-zero when any file failed or when the sweep was interrupted before every file was visited.

Example:

```bash
hs scribe inbox sweep
```

### `hs scribe inbox install`

Install a user-level daemon that runs the inbox watcher at login. macOS: LaunchAgent plist. Linux: systemd --user unit.

```text
hs scribe inbox install [OPTIONS]
```

The command takes no arguments or flags of its own.

Writes the user-level service for `hs scribe inbox run` and starts it. On macOS the service is the LaunchAgent `io.home-still.scribe-inbox` (`~/Library/LaunchAgents/io.home-still.scribe-inbox.plist`). On Linux it is the user unit `home-still-scribe-inbox.service` (`~/.config/systemd/user/`).

Example:

```bash
hs scribe inbox install
```

### `hs scribe inbox uninstall`

Remove the user-level daemon.

```text
hs scribe inbox uninstall [OPTIONS]
```

The command takes no arguments or flags of its own.

Stops and removes the service that `hs scribe inbox install` created.

Example:

```bash
hs scribe inbox uninstall
```

### `hs scribe inbox status`

Report the daemon's running state.

```text
hs scribe inbox status [OPTIONS]
```

The command takes no arguments or flags of its own.

Reports the service's running state.

Example:

```bash
hs scribe inbox status
```

### `hs scribe catalog-backfill`

Create catalog entries for markdown files in `scribe.output_dir` that have none.

```text
hs scribe catalog-backfill [OPTIONS]
```

The command takes no arguments or flags of its own.

Works on the local directories `scribe.output_dir` (markdown), `scribe.catalog_dir` (catalog) and `scribe.watch_dir` (PDFs). For each markdown file without a catalog entry it writes one with the title taken from the first non-empty line, the source PDF path when the PDF exists, and the page accounting. It prints how many entries were created, already existed or were unreadable, and exits non-zero when any file was unreadable.

Example:

```bash
hs scribe catalog-backfill
```

### `hs scribe reconvert`

Clear a document's conversion stamps and derived artifacts, then republish `papers.ingested` so the watcher converts it again.

```text
hs scribe reconvert [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Catalog stem (no extension), e.g. `10.48550_arxiv.2312.10997`. |

Resets one document so the watcher converts it again. The steps, in order:

1. Validate the stem, read its catalog entry, and find its source (`.pdf`, `.html`, `.htm` or `.epub`) under `papers/`.
2. When the document has an embedding or an embedding-skip stamp, delete its records from its store through the distill server. A failure here stops the command before anything else changes.
3. Delete the markdown object.
4. Clear the `conversion`, `conversion_failed`, `embedding` and `embedding_skip` stamps.
5. Publish `papers.ingested` for the source.

The command fails when the stem has no catalog entry, when the entry has neither a `conversion` nor a `conversion_failed` stamp, or when no source exists. It has no dry run and no confirmation prompt.

Example:

```bash
hs scribe reconvert 10.48550_arxiv.2312.10997
```

### `hs scribe queue`

Announce that one stored source needs converting.

```text
hs scribe queue [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Catalog stem (no extension), e.g. `10.48550_arxiv.2312.10997`. |

Looks for the stem's source (`.pdf`, `.html` or `.epub`) in the `academic` store and publishes `papers.ingested` for it, so the scribe watcher converts it. It converts nothing itself, and a stem with no stored source is an error that names every key tried and publishes nothing. It needs `storage:` and an `events:` section, and fails when the event bus is not reachable. On success it prints a JSON report with `stem`, `source_key`, `queued` and a hint for what to run next. Unlike `hs scribe reconvert`, it clears nothing: it is for a source that was stored and never announced.

Example:

```bash
hs scribe queue 10.48550_arxiv.2312.10997
```

### `hs scribe boxes`

Word-box sidecars: where every word of a PDF sits on its page, which is what lets a quotation be highlighted in the document it is from.

```text
hs scribe boxes [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs scribe boxes catch-up` | Queue a `scribe.boxes` build for every converted PDF that has no word-box sidecar. The running `hs scribe watch-events` builds them. |
| `hs scribe boxes status` | Per store: PDFs, converted PDFs, sidecars built and sidecars missing; then the depth of the `scribe.boxes` queue. |

A sidecar is one object per PDF, `<boxes prefix>/<shard>/<stem>.words.json.gz`: every word of every page with its box, read from the PDF's text layer, or by the word OCR for a page that has none. The stores that hold PDFs are `academic` (boxes under `boxes/`) and `personal` (under `<personal.storage_dir>/boxes/`).

Neither command builds anything. A build is a `scribe.boxes` event, and `hs scribe watch-events` consumes it. The event is published when a PDF's markdown is stored, by `hs personal add` for a PDF, and by `hs scribe boxes catch-up`.

Example:

```bash
hs scribe boxes status
```

### `hs scribe boxes catch-up`

Queue a `scribe.boxes` build for every converted PDF that has no word-box sidecar. The running `hs scribe watch-events` builds them.

```text
hs scribe boxes catch-up [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--store <STORE>` | none | Only this document store (default: every store that holds PDFs). |
| `--dry-run` | off | Print the storage key of each PDF that would be queued, one per line, and publish nothing. |

Lists each store's PDFs, markdown and sidecars, and publishes one `scribe.boxes` event for every PDF that has markdown and no sidecar. A PDF that is not converted yet is left out: its conversion asks for the word boxes when it finishes. `--store` must name a store that holds PDFs (`academic` or `personal`); any other store is refused. The command needs `storage:` and, without `--dry-run`, `events:`.

The command does not read the queue. Run again while earlier requests are still waiting, it queues those PDFs a second time, and the second build replaces the sidecar with an identical one; check `hs scribe boxes status` first. `--dry-run` writes the keys to standard output.

It ends with `boxes_requested=<n> errors=<n>` and exits non-zero when a request could not be published or the run was interrupted.

Example:

```bash
hs scribe boxes catch-up --store academic --dry-run
```

### `hs scribe boxes status`

Per store: PDFs, converted PDFs, sidecars built and sidecars missing; then the depth of the `scribe.boxes` queue.

```text
hs scribe boxes status [OPTIONS]
```

The command takes no arguments or flags of its own.

Prints one line for each store that holds PDFs: its PDFs, how many of them are converted, how many have word boxes, and how many converted PDFs are missing them. The last line sums up: `missing` over all stores, `queued` (requests waiting on the `scribe.boxes` queue) and `building` (requests a watcher has taken and not finished).

The command needs `storage:` and `events:`. It prints the store lines first and then fails if the queue cannot be read, which is the case until an `hs scribe watch-events` of this version has started once and created the queue's consumer.

Example:

```bash
hs scribe boxes status
```

## `hs paper`

Academic paper search, lookup, and download.

```text
hs paper [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs paper search` | Search for papers across providers. |
| `hs paper get` | Get a single paper by DOI. |
| `hs paper download` | Download papers (search + download, or single DOI). |
| `hs paper config` | View and manage configuration. |

The `paper` commands read the `paper:`, `storage:` and `events:` sections. See [configuration](configuration.md) and [paper providers](paper-providers.md).

Example:

```bash
hs paper --help
```

### `hs paper search`

Search for papers across providers.

```text
hs paper search [OPTIONS] <QUERY>
```

| Name | Default | Meaning |
|---|---|---|
| `<QUERY>` | required | Search query string. |
| `-t`, `--type <SEARCH_TYPE>` | `keywords` | Type of search to perform. One of: `keywords`, `title`, `author`, `doi`, `subject`. |
| `-a`, `--abstract` | off | Show abstracts in search results. |
| `-d`, `--date <DATE>` | none | Filter by date range (e.g., ">=2025", ">2023 <2025", ">=2024-06"). |
| `-n`, `--max-results <MAX_RESULTS>` | `10` | Maximum number of results. |
| `--offset <OFFSET>` | `0` | Pagination offset. OpenAlex needs a multiple of the page size; Europe PMC cannot page by offset (rejected alone, skipped in `all`). |
| `-p`, `--provider <PROVIDER>` | `all` | Provider to search. One of: `all`, `arxiv`, `openalex`, `semanticscholar`, `europepmc`, `crossref`, `core`. |
| `-s`, `--sort <SORT_BY>` | `relevance` | Sort result by: relevance (default; blended score), date (newest first) or citations (most cited first). date/citations are strict orders, papers with no date/count last; citations also drops papers that do not match the query terms. One of: `relevance`, `date`, `citations`. |
| `--min-citations <MIN_CITATIONS>` | none | Minimum citation count to include a result. |

When the query is a DOI or an arXiv identifier, the command looks that paper up instead of searching. Provider names accept the aliases `semantic_scholar` and `s2` for `semanticscholar`, and `europe_pmc`, `europmc` and `pmc` for `europepmc`. See [paper providers](paper-providers.md) for coverage, rate limits and query syntax.

The command exits with code 3 when the providers cannot be reached (see [exit codes](#exit-codes-and-interruption)). With `--output json` it prints the result as JSON.

Example:

```bash
hs paper search "transformer attention" -n 5 --sort citations --date ">=2023"
```

### `hs paper get`

Get a single paper by DOI.

```text
hs paper get [OPTIONS] --doi <DOI>
```

| Name | Default | Meaning |
|---|---|---|
| `--doi <DOI>` | required | DOI to look up. |
| `-p`, `--provider <PROVIDER>` | `all` | Provider to query. `all` (default) asks every provider; an arXiv DOI goes to arXiv alone, since no other provider indexes it. One of: `all`, `arxiv`, `openalex`, `semanticscholar`, `europepmc`, `crossref`, `core`. |

The command exits with code 1 when the paper is not found. With `--output json` it prints the paper as JSON.

Example:

```bash
hs paper get --doi "10.48550/arXiv.2301.00001"
```

### `hs paper download`

Download papers (search + download, or single DOI).

```text
hs paper download [OPTIONS] [QUERY]
```

| Name | Default | Meaning |
|---|---|---|
| `<QUERY>` | none | Search query (downloads matching papers). |
| `-d`, `--date <DATE>` | none | Filter by date range (e.g., ">=2025", ">2023 <2025", ">=2024-06"). |
| `--doi <DOI>` | none | Download a single paper by DOI. |
| `-n`, `--max-results <MAX_RESULTS>` | `10` | Maximum number of papers to download. |
| `-c`, `--concurrency <CONCURRENCY>` | `4` | Maximum concurrent downloads. |
| `-t`, `--type <SEARCH_TYPE>` | `keywords` | Search type for query-based download. One of: `keywords`, `title`, `author`, `doi`, `subject`. |
| `-p`, `--provider <PROVIDER>` | `all` | Provider to search. One of: `all`, `arxiv`, `openalex`, `semanticscholar`, `europepmc`, `crossref`, `core`. |

Pass a query to download the best matches, or `--doi` to download one paper; the two cannot be combined. The command stores each PDF in the configured storage as `papers/<shard>/<stem>.pdf` and publishes `papers.ingested`, so it needs an `events:` section in the configuration: with none, the command refuses to run. When publishing fails, the PDF stays stored and the error names `hs pipeline catch-up` as the way to queue it.

Example:

```bash
hs paper download "neural nets" -n 25 --provider openalex
```

### `hs paper config`

View and manage configuration.

```text
hs paper config [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs paper config show` | Print the resolved configuration. |
| `hs paper config path` | Print the config file path. |

Shows the resolved configuration of the `paper` commands.

Example:

```bash
hs paper config show
```

### `hs paper config show`

Print the resolved configuration.

```text
hs paper config show [OPTIONS]
```

The command takes no arguments or flags of its own.

Example:

```bash
hs paper config show
```

### `hs paper config path`

Print the config file path.

```text
hs paper config path [OPTIONS]
```

The command takes no arguments or flags of its own.

Example:

```bash
hs paper config path
```

## `hs personal`

Personal documents: ingest medical/financial/etc records into a private store.

```text
hs personal [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs personal add` | Ingest a single file into the personal store. |
| `hs personal list` | List ingested documents, most recently ingested first. |
| `hs personal search` | Semantic search across the `personal` store. |
| `hs personal read` | Read a document's converted markdown by stem. |
| `hs personal delete` | Remove a document from the `personal` store and disk. |
| `hs personal reindex` | Re-chunk and re-embed a document. Useful after the chunker changes. |
| `hs personal config` | View configuration. |

The personal documents are a store of their own, `personal`, apart from the `academic` store; see [configuration](configuration.md) for the `personal:` section.

Example:

```bash
hs personal --help
```

### `hs personal add`

Ingest a single file into the personal store.

```text
hs personal add [OPTIONS] <FILE>
```

| Name | Default | Meaning |
|---|---|---|
| `<FILE>` | required | Path to the file to ingest. |
| `--category <CATEGORY>` | none | Override the LLM-picked category. Must be one of the configured categories. |
| `--title <TITLE>` | none | Override the LLM-picked title. |
| `--force` | off | Replace an existing document with the same stem or the same file content (the old document, markdown, sidecar and vectors are removed once the new ingest succeeds). Without it, a file whose content is already stored is refused. |

Accepted formats are PDF, EPUB, DOCX, Markdown (`.md`, `.markdown`) and plain text (`.txt`). A language model picks the category and title unless you pass them. A file whose content is already stored is refused unless you pass `--force`.

Adding a PDF publishes `scribe.boxes` once the document is stored and indexed, and the scribe watcher builds its word boxes afterwards. A PDF therefore needs the `events:` section and a reachable broker; without them it is refused before it is converted.

Example:

```bash
hs personal add tax-2024.docx --category tax
```

### `hs personal list`

List ingested documents, most recently ingested first.

```text
hs personal list [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--category <CATEGORY>` | none | Filter by category (one of the configured categories; any case). |
| `-n`, `--limit <LIMIT>` | `50` | Maximum number of results. |

Example:

```bash
hs personal list --category medical -n 20
```

### `hs personal search`

Semantic search across the `personal` store.

```text
hs personal search [OPTIONS] <QUERY>
```

| Name | Default | Meaning |
|---|---|---|
| `<QUERY>` | required | Query string. |
| `--category <CATEGORY>` | none | Filter by category (one of the configured categories; any case). |
| `-n`, `--limit <LIMIT>` | `10` | Maximum number of results. |

Example:

```bash
hs personal search "blood pressure" --category medical
```

### `hs personal read`

Read a document's converted markdown by stem.

```text
hs personal read [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Document stem (filename without extension). |

Example:

```bash
hs personal read lab-results
```

### `hs personal delete`

Remove a document from the `personal` store and disk.

```text
hs personal delete [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Document stem. |

Removes the document's records from the `personal` store and the document from disk, with its word boxes. The command has no confirmation prompt and no dry run.

Example:

```bash
hs personal delete lab-results
```

### `hs personal reindex`

Re-chunk and re-embed a document. Useful after the chunker changes, or to fill an empty index.

```text
hs personal reindex [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Document stem. |

Example:

```bash
hs personal reindex lab-results
```

### `hs personal config`

View configuration.

```text
hs personal config [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs personal config show` | Print the resolved configuration. |
| `hs personal config path` | Print the config file path. |

Example:

```bash
hs personal config show
```

### `hs personal config show`

Print the resolved configuration.

```text
hs personal config show [OPTIONS]
```

The command takes no arguments or flags of its own.

Example:

```bash
hs personal config show
```

### `hs personal config path`

Print the config file path.

```text
hs personal config path [OPTIONS]
```

The command takes no arguments or flags of its own.

Example:

```bash
hs personal config path
```

## `hs distill`

Distill markdown into vector embeddings for semantic search.

```text
hs distill [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs distill search` | Semantic search across indexed documents. |
| `hs distill status` | Show distill system status (server, backend, the `academic` store). |
| `hs distill watch-events` | Consume `scribe.completed` events from the event bus and index each markdown object through the distill server. |
| `hs distill diagnose` | Explain why a stem produces or does not produce chunks: fetch its markdown, run the chunker and quality filter, and print the accept or reject reason for each chunk. |
| `hs distill reconcile` | Compare the markdown objects, the `academic` store and the catalog, and optionally repair the differences. |
| `hs distill reindex` | Index documents of the `academic` store again from their stored markdown and catalog entry, replacing their records in place. |
| `hs distill backfill` | Index the converted documents that were never embedded. |
| `hs distill scan-repetitions` | Report stored markdown that the repetition cleaner would cut heavily. |
| `hs distill purge` | Delete every stored chunk for a `doc_id`. |
| `hs distill abstracts` | Manage the `abstracts` store, which holds one record per paper built from its title and abstract. |

The distill commands talk to the distill server named by `distill.servers` and read `storage:`; the watcher also needs `events:`. See [configuration](configuration.md).

Example:

```bash
hs distill --help
```

### `hs distill search`

Semantic search across indexed documents.

```text
hs distill search [OPTIONS] <QUERY>
```

| Name | Default | Meaning |
|---|---|---|
| `<QUERY>` | required | Search query text. |
| `-l`, `--limit <LIMIT>` | `10` | Number of results. |
| `--year <YEAR>` | none | Filter by year (e.g., ">2020"). |
| `--topic <TOPIC>` | none | Filter by topic. |
| `--store <NAME>` | none | Store to search: `academic`, `abstracts`, `references`, `personal`, `web` or `syntheses`. Repeat the flag to search several. Without it the server searches its default stores, which is `academic`. |
| `--server <SERVER>` | none | Override server URL. |

Prints ranked chunks with title, authors, year, the store and document each one is from, its lines and page, and a text preview. `--output json` prints the hits as one JSON array; `--output ndjson` prints one hit per line. An empty query is an error, and so is a store the server does not have.

Example:

```bash
hs distill search "attention mechanisms" --year ">2020" --limit 5
hs distill search "transformer architectures survey" --store abstracts
```

### `hs distill status`

Show distill system status (server, backend, the `academic` store).

```text
hs distill status [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--server <SERVER>` | none | Override server URL. |

Prints the server's state, then the `academic` store's chunk count and the device the embedding model runs on.

Example:

```bash
hs distill status
```

### `hs distill watch-events`

Consume `scribe.completed` events from the event bus and index each markdown object through the distill server.

```text
hs distill watch-events [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--server <SERVER>` | none | Override server URL. |

Runs until stopped. It needs `storage:`, `events:` and `distill.servers`; the first server in `distill.servers` is used unless you pass `--server`. Before it pulls an event, it checks that the server accepts its `HS_BACKEND_TOKEN`. Run it as a service with `hs serve distill-watch --install`.

Each event names its store: the document is indexed into that store and its catalog entry there is stamped. An event that names no store, a store that keeps no documents of its own, or a key that is not that store's markdown key for the document is dropped and is not delivered again.

Example:

```bash
hs distill watch-events
```

### `hs distill diagnose`

Explain why a stem produces or does not produce chunks: fetch its markdown, run the chunker and quality filter, and print the accept or reject reason for each chunk.

```text
hs distill diagnose [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | Paper stem (filename without extension). |
| `--verbose` | off | Print the full text of each chunk. Replaces the global `--verbose` on this command. |

Reads the markdown from storage and runs the same chunker and quality filter the indexer uses. Nothing is written.

Example:

```bash
hs distill diagnose 10.48550_arxiv.2312.10997 --verbose
```

### `hs distill reconcile`

Compare the markdown objects, the `academic` store and the catalog, and optionally repair the differences.

```text
hs distill reconcile [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--fix-stamps` | off | Backfill missing `embedding` stamps for docs already in the store. |
| `--reembed` | off | Re-index markdown that never reached the store. |
| `--server <SERVER>` | none | Override server URL. |

Without flags the command only reports counts and examples: the documents that are `ok`, those missing their `embedding` stamp (embedded but unstamped), and those whose markdown never reached the store. `--fix-stamps` writes the missing stamps. `--reembed` re-indexes the markdown that never reached the store. The command lists every markdown object, every indexed `doc_id` and every catalog entry, so it takes time on a large corpus.

Example:

```bash
hs distill reconcile --fix-stamps
```

### `hs distill reindex`

Index documents of the `academic` store again from their stored markdown and catalog entry, replacing their records in place.

```text
hs distill reindex [OPTIONS] [STEMS]...
```

| Name | Default | Meaning |
|---|---|---|
| `[STEMS]...` | none | Stems of the documents to index again. Required unless `--all` is given. |
| `--all` | off | Index every document that has markdown. |
| `--older-than <TIME>` | none | With `--all`, leave out the documents indexed at or after this time (RFC 3339). |
| `--concurrency <CONCURRENCY>` | `1` | How many documents are indexed at once. |
| `--server <SERVER>` | none | Override server URL. |

A document's new records are written before its earlier ones are deleted, so it stays searchable throughout. Each document's catalog entry is read again, which picks up corrected metadata, and its `embedding` stamp is written again. A document that fails is reported and counted and keeps the records and the stamp it had; the others still run, and the command exits non-zero.

Use `--all` after an upgrade that changes what a record carries. A run over a large store takes hours: to continue one that stopped, pass the time it began as `--older-than`, and the documents it already indexed are left out. A document the catalog marks with a failed index attempt is never left out.

Example:

```bash
hs distill reindex --all --older-than 2026-01-31T00:00:00Z
```

### `hs distill backfill`

Index the converted documents that were never embedded.

```text
hs distill backfill [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be indexed without sending anything to the server or writing anything. |
| `--limit <LIMIT>` | none | Index at most this many documents. Without it, all of them. |
| `--retry-skipped` | off | Also index documents whose last indexing found nothing to index. |
| `--server <SERVER>` | none | Override server URL. |

A document is a candidate when its catalog entry has a conversion stamp, no embedding stamp with chunks, and no record that indexing found nothing to index (unless `--retry-skipped` is given). The command indexes the candidates one at a time into the `academic` store and stamps each catalog entry; the JSON report has `candidates`, `indexed`, `still_skipped` (indexed with no chunks), `errors` (one line for each document that could not be indexed or stamped), up to ten `samples` and `dry_run: false`. With `--dry-run` it sends nothing and writes nothing, and the report has `candidates`, `would_index`, `samples` and `dry_run: true`. The command exits with an error when `errors` is not empty, after printing the report. `hs distill reconcile --reembed` also indexes unembedded documents, and it compares the store as well; `hs distill reindex` indexes documents again whether or not they were embedded.

Example:

```bash
hs distill backfill --dry-run --limit 100
```

### `hs distill scan-repetitions`

Report stored markdown that the repetition cleaner would cut heavily.

```text
hs distill scan-repetitions [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--limit <LIMIT>` | `100000` | Scan at most this many documents. |
| `--threshold <THRESHOLD>` | `20` | Report documents cut more than this many times. |

A converter that loops writes the same words again and again. The command runs the repetition cleaner over each stored markdown document of the `academic` store in memory and reports the documents it would cut more than `--threshold` times. The JSON report has `scanned`, `threshold`, `flagged_count` and `flagged`; each flagged document has its `stem`, its storage `key`, `truncations`, the count for each cleaning pass in `truncations_by_pass` and an `offending_snippet` where the cleaned text first differs. The command changes nothing. It stops with an error naming the document when one cannot be read or is not UTF-8.

Example:

```bash
hs distill scan-repetitions --threshold 50
```

### `hs distill purge`

Delete every stored chunk for a `doc_id`.

```text
hs distill purge [OPTIONS] <DOC_ID>
```

| Name | Default | Meaning |
|---|---|---|
| `<DOC_ID>` | required | doc_id whose chunks should be deleted. |
| `--server <SERVER>` | none | Override server URL. |

The command is CLI-only because it is a destructive write. It has no dry run and no confirmation prompt: it asks the distill server to delete the document's chunks from the `academic` store, and its reference lists with them, and prints how many were deleted. The markdown, the source, the catalog entry and the paper's record in the `abstracts` store are not touched.

Example:

```bash
hs distill purge 10.48550_arxiv.2312.10997
```

### `hs distill abstracts`

Manage the `abstracts` store, which holds one record per paper built from its title and abstract.

```text
hs distill abstracts [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs distill abstracts build` | Embed the title and abstract of every catalog entry into the `abstracts` store and stamp the catalog. |
| `hs distill abstracts reconcile` | Embed only the catalog entries that have no `abstract_embed` stamp. |
| `hs distill abstracts status` | Report abstract coverage from the catalog stamps, broken down by source. |

Example:

```bash
hs distill abstracts --help
```

### `hs distill abstracts build`

Embed the title and abstract of every catalog entry into the `abstracts` store and stamp the catalog.

```text
hs distill abstracts build [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--force` | off | Re-embed entries that already have an `abstract_embed` stamp. |
| `--server <SERVER>` | none | Override server URL. |

For each catalog entry the abstract comes from the local OpenAlex catalog (looked up by DOI), else from the catalog's provider abstract, else from the `## Abstract` section of the markdown; entries with none of them are counted as `no_abstract`. The command opens the OpenAlex database named by `openalex.db_path` and fails without it. Entries that already carry an `abstract_embed` stamp are skipped unless you pass `--force`. The command exits non-zero when any entry failed or when it was interrupted; re-run to continue.

Example:

```bash
hs distill abstracts build --force
```

### `hs distill abstracts reconcile`

Embed only the catalog entries that have no `abstract_embed` stamp.

```text
hs distill abstracts reconcile [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--server <SERVER>` | none | Override server URL. |

Same as `hs distill abstracts build` without `--force`. It is for incremental catch-up after new papers arrive.

Example:

```bash
hs distill abstracts reconcile
```

### `hs distill abstracts status`

Report abstract coverage from the catalog stamps, broken down by source.

```text
hs distill abstracts status [OPTIONS]
```

The command takes no arguments or flags of its own.

Reads only the catalog, never the store, so it is safe to run while the indexer is busy. It prints the catalog size, the number of stamped entries by source (`openalex`, `catalog`, `markdown`, `title_only`) and the number of unstamped entries.

Example:

```bash
hs distill abstracts status
```

## `hs docs`

Signed, expiring links to the documents in the corpus.

```text
hs docs [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs docs link` | Mint a link to a document. |
| `hs docs prune` | Delete the short links whose signed link has expired. |

Links are signed with the key in `~/.home-still/` and checked by the docs service started with `hs serve docs`. See [configuration](configuration.md) for the `docs:` section.

Example:

```bash
hs docs --help
```

### `hs docs link`

Mint a link to a document.

```text
hs docs link [OPTIONS] <STEM>
```

| Name | Default | Meaning |
|---|---|---|
| `<STEM>` | required | The document's stem. |
| `--markdown` | off | Link to the converted markdown instead of the original document. |
| `--collection <COLLECTION>` | `academic` | The store the document is in (the flag keeps its name): `academic`, `personal`, `web` or `syntheses`. A name that is not a store is refused. The store is part of the link's signature. |
| `--page <PAGE>` | none | The page to open, 1-based. The link opens that page and points at it. |
| `--subject <SUBJECT>` | none | Subject the link is bound to. Default: `docs.default_subject`, else this machine's host name. |
| `--ttl-secs <TTL_SECS>` | none | Seconds the link works. Default: `docs.link_ttl_secs`, else 3600. |
| `--base-url <BASE_URL>` | none | Base URL the link points at. Default: `docs.lan_base_url`. |
| `--remote` | off | Also print the link under `docs.remote_base_url`. |
| `--short` | off | Mint a short link — `<base>/s/<code>` — instead of the full signed URL. |

Prints the LAN link on its own line, then the remote link when `--remote` is set and `docs.remote_base_url` is configured, then the expiry time. The links go to **stdout** and the expiry to stderr, so the URLs can be piped or captured (`hs docs link <stem> | xclip`); `--quiet` suppresses only the expiry. The link needs `docs.lan_base_url` in the configuration or `--base-url`. The signing key is created on first use. A link with `--page` carries the page twice: in the signed query string and in the URL fragment the browser uses to scroll.

With `--short` a 12-character code is stored in the corpus and printed as `<base>/s/<code>` instead. The code is an alias: the docs service answers it with a redirect to the same signed link `hs docs link` would have printed, carrying the `--page` fragment, so nothing the signature protects changes. The redirect's `Location` is a path, so one code works on the LAN and away from home. It expires with the signed link and `hs docs prune` deletes it afterwards; storing it needs the `storage:` section.

Example:

```bash
hs docs link 10.48550_arxiv.2312.10997 --markdown --page 3 --remote
hs docs link 10.48550_arxiv.2312.10997 --short --remote
```

### `hs docs prune`

Delete the short links whose signed link has expired.

```text
hs docs prune
```

A short link is a record in the corpus: a code, the path it redirects to, the fragment, and the expiry of the link behind it. Once that link has expired the record is a redirect into a 403, so it is garbage. The command walks the short-link prefix and deletes every expired record, printing how many it removed. It never deletes a live code, and it needs the `storage:` section.

## `hs status`

Live dashboard — pipeline health, services, recent activity.

```text
hs status [OPTIONS]
```

The command takes no arguments or flags of its own.

On an interactive terminal the command opens a full-screen dashboard that refreshes every 3 seconds; press `q`, `Esc` or Ctrl+C to leave. Without a terminal it prints the same data once as text. `--output json` prints one pretty-printed JSON document and `--output ndjson` prints it on one line. Every row is described in [status dashboard](status-dashboard.md).

Example:

```bash
hs status --output json
```

## `hs restart`

Restart all running services (system services, daemons, containers).

```text
hs restart [OPTIONS]
```

The command takes no arguments or flags of its own.

Discovers the units that run this host's home-still binaries (systemd system units, systemd user units and launchd agents), matches them by the executable they run, and restarts them. It also restarts the scribe compose containers (`~/.home-still/docker-compose.yml`, when `scribe.local_server` is on), and exits non-zero when any restart failed. One-shot timer jobs are left alone.

Example:

```bash
hs restart
```

## `hs upgrade`

Check for a newer release and upgrade `hs`, its companion binaries and the managed services.

```text
hs upgrade [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--check` | off | Only check if an update is available; do not install. |
| `--force` | off | Force reinstall even if already on the latest version. |
| `--pre` | off | Include pre-release versions (e.g. rc candidates). |

The command fetches the latest release from GitHub, asks for confirmation (skipped by `--yes`), and resolves every download and checksum before it changes anything. It then updates the scribe compose stack, replaces the installed binaries with `hs` last, restarts the services that run the replaced binaries, and waits for them to answer their health endpoints.

The companion binaries `hs-distill-server`, `hs-gateway`, `hs-mcp` and `hs-scribe-server` are upgraded only when already installed on the host. For the scribe compose stack (`~/.home-still/docker-compose.yml`, when `scribe.local_server` is on) the command pulls the images and recreates the containers only when an image changed or the service does not answer its health URL; an unchanged, healthy service keeps running. It does not manage myelin or its Qdrant, and it waits for no database after the restart. Without `--pre`, only stable releases are considered.

Example:

```bash
hs upgrade --pre --yes
```

## `hs serve`

Run a home-still service on this machine: in the foreground, as an installed service, or in the background.

```text
hs serve [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs serve scribe` | Run a scribe server (auto-init, foreground). |
| `hs serve distill` | Run a distill server (auto-init, foreground). |
| `hs serve mcp` | Run an MCP server (foreground). |
| `hs serve scribe-watch` | NATS event-watch daemon that converts `papers.ingested` events into markdown via the scribe pool. Runs as a user-level service. |
| `hs serve distill-watch` | NATS event-watch daemon that indexes `scribe.completed` events into the myelin store via the distill server. Runs as a user-level service. |
| `hs serve docs` | Run the docs service: serves the corpus's documents over signed, expiring links. |

`hs serve` runs a service in the foreground, installs it as a service, or starts and stops it in the background. The ports are listed in [services and ports](services-and-ports.md).

Example:

```bash
hs serve --help
```

### `hs serve scribe`

Run a scribe server (auto-init, foreground).

```text
hs serve scribe [OPTIONS] [ACTION]
```

| Name | Default | Meaning |
|---|---|---|
| `<ACTION>` | none | `start` runs the scribe compose project in the background, `stop` stops it. Without it the server runs in the foreground. |
| `--port <PORT>` | `7433` | Port the foreground server and the installed service listen on. `start` and `stop` ignore it. |
| `--install` | off | Install as a system service (systemd on Linux, launchd on macOS) and start it. |
| `--uninstall` | off | Stop and remove the system service. |

In the foreground the command runs the `hs-scribe-server` binary on `0.0.0.0:<port>`. It refuses to run when `scribe.local_server` is `false`, and when a system service of the same name is already active.

`--install` writes the system unit `hs-serve-scribe.service` (Linux, using `sudo`) or the LaunchAgent `com.home-still.scribe` (macOS), enables it and starts it. `--uninstall` removes it. `--install` and `--uninstall` cannot be combined.

`start` and `stop` manage `~/.home-still/docker-compose.yml`; when that file does not exist, `start` and `stop` fail and tell you to use `hs serve scribe`.

Example:

```bash
hs serve scribe --port 7433
```

### `hs serve distill`

Run a distill server (auto-init, foreground).

```text
hs serve distill [OPTIONS] [ACTION]
```

| Name | Default | Meaning |
|---|---|---|
| `<ACTION>` | none | `start` runs the distill server in the background, `stop` stops it. Without it the server runs in the foreground. |
| `--port <PORT>` | `7434` | Port the foreground server and the installed service listen on. `start` and `stop` use `distill_server.port` from the configuration instead. |
| `--install` | off | Install as a system service and start it. |
| `--uninstall` | off | Stop and remove the system service. |

In the foreground the command runs the `hs-distill-server` binary on the given port. The server needs a myelin server at `distill_server.myelin_url` and Ollama at `distill_server.ollama_url`; it stops at start-up when myelin refuses one of its [stores](configuration.md#stores), and answers 503 on `/health` until the embedding model is on the GPU.

`--install` writes the system unit `hs-serve-distill.service` (Linux, using `sudo`) or the LaunchAgent `com.home-still.distill` (macOS), enables it and starts it. `--uninstall` removes it.

`start` stops any distill server recorded in the PID file, starts `hs-distill-server` in the background with its output in `distill-server.log` under the log directory, and waits up to 300 seconds for `/health`. It starts no database: myelin and Ollama must already be running. `stop` stops the server process.

Example:

```bash
hs serve distill start
```

### `hs serve mcp`

Run an MCP server (foreground).

```text
hs serve mcp [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--port <PORT>` | `7445` | Port to listen on. |
| `--install` | off | Install as a system service and start it. |
| `--uninstall` | off | Stop and remove the system service. |

Runs `hs-mcp --serve 0.0.0.0:<port>` and forwards a stop request to it. `--install` writes the system unit `hs-serve-mcp.service` (Linux, using `sudo`) or the LaunchAgent `com.home-still.mcp` (macOS), enables it and starts it. The command fails when the `hs-mcp` binary is not installed.

Example:

```bash
hs serve mcp --install
```

### `hs serve scribe-watch`

NATS event-watch daemon that converts `papers.ingested` events into markdown via the scribe pool. Runs as a user-level service.

```text
hs serve scribe-watch [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--install` | off | Install as a user service (systemd --user on Linux, LaunchAgent on macOS) and start it. Runs under the current user — no sudo. |
| `--uninstall` | off | Stop and remove the user service. |

Without a flag the command runs `hs scribe watch-events` in the foreground. `--install` writes the user unit `hs-scribe-watch-events.service` (Linux, `systemd --user`) or the LaunchAgent `com.home-still.scribe-watch-events` (macOS), enables it and starts it. It runs as the current user and needs no `sudo`.

Example:

```bash
hs serve scribe-watch --install
```

### `hs serve distill-watch`

NATS event-watch daemon that indexes `scribe.completed` events into the myelin store via the distill server. Runs as a user-level service.

```text
hs serve distill-watch [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--install` | off | Install as a user service and start it. |
| `--uninstall` | off | Stop and remove the user service. |

Without a flag the command runs `hs distill watch-events` in the foreground. `--install` writes the user unit `hs-distill-watch-events.service` (Linux) or the LaunchAgent `com.home-still.distill-watch-events` (macOS), enables it and starts it. It runs as the current user and needs no `sudo`.

Example:

```bash
hs serve distill-watch --install
```

### `hs serve docs`

Run the docs service: serves the corpus's documents over signed, expiring links.

```text
hs serve docs [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--port <PORT>` | `7446` | Port to listen on. |
| `--install` | off | Install as a system service and start it. |
| `--install-user` | off | Install as a user service and start it. |
| `--uninstall` | off | Stop and remove the service. |

In the foreground the command serves the corpus's documents on `0.0.0.0:<port>` over signed, expiring links. It requires a `storage:` section, creates the link signing key on first use (mode 0600, never overwritten), and reads the revocation list the gateway also reads, so revoking a device revokes its links. It serves the documents of every store (`academic`, `personal`, `web`, `syntheses`) whose objects are in storage.

`--install` writes the system unit `hs-serve-docs.service` (Linux, using `sudo`) or the LaunchAgent `com.home-still.docs` (macOS). `--install-user` writes the user unit `hs-serve-docs.service` (Linux) or the LaunchAgent `com.home-still.serve-docs` (macOS). `--uninstall` removes both kinds. The three flags cannot be combined.

Example:

```bash
hs serve docs --install --port 7446
```

## `hs cloud`

Remote cloud access — enrollment, gateway management.

```text
hs cloud [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs cloud init` | Initialize this node as the cloud gateway. |
| `hs cloud invite` | Generate a one-time enrollment code for a new device (run on the gateway host). |
| `hs cloud revoke` | Revoke every token issued to a device (run on the gateway host). |
| `hs cloud enroll` | Enroll this device with a cloud gateway. |
| `hs cloud status` | Show cloud connection status. |
| `hs cloud token` | Print a fresh access token (for MCP server config, CI, etc.). |

The gateway side (`init`, `invite`, `revoke`) runs on the host that runs `hs-gateway`; the device side (`enroll`, `status`, `token`) runs on the devices that connect to it. See [configuration](configuration.md) for `cloud.gateway`.

Example:

```bash
hs cloud --help
```

### `hs cloud init`

Initialize this node as the cloud gateway.

```text
hs cloud init [OPTIONS]
```

The command takes no arguments or flags of its own.

Creates the gateway's signing secret and admin key (mode 0600) when they do not exist, at the path `cloud.gateway.secret_path` or its default. Existing files are kept; a malformed existing file is an error and is never regenerated.

Example:

```bash
hs cloud init
```

### `hs cloud invite`

Generate a one-time enrollment code for a new device (run on the gateway host).

```text
hs cloud invite [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--name <NAME>` | `device` | Device name for the enrollment. |
| `--scope <SCOPE>` | none | Scope to grant: `scribe`, `distill`, `mcp` or `memory`. Repeat the flag for several. Without it the code grants all four. |

Asks the local gateway for a one-time enrollment code and prints the code, its scopes and its lifetime. It reads `cloud.gateway.listen` and the admin key on this host, so it works only on the gateway host, and it fails when `hs-gateway` is not running.

Example:

```bash
hs cloud invite --name laptop --scope mcp
```

### `hs cloud revoke`

Revoke every token issued to a device (run on the gateway host).

```text
hs cloud revoke [OPTIONS] --name <NAME>
```

| Name | Default | Meaning |
|---|---|---|
| `--name <NAME>` | required | Device name, or `oauth:<client_id>` for an OAuth client. |

Revokes every token issued to the device or OAuth client, and the signed document links minted for it. Re-enroll the device with `hs cloud invite`. The command has no confirmation prompt. Run it on the gateway host.

Example:

```bash
hs cloud revoke --name laptop
```

### `hs cloud enroll`

Enroll this device with a cloud gateway.

```text
hs cloud enroll [OPTIONS] --gateway <GATEWAY>
```

| Name | Default | Meaning |
|---|---|---|
| `--gateway <GATEWAY>` | required | Gateway URL (e.g., https://cloud.example.com). |

The gateway URL must be a bare `https` origin (`https://host[:port]`, no path, query or credentials). The command prompts for the enrollment code, then saves the credentials in `~/.home-still/cloud-token`. Conversions route through the gateway when its URL is in `scribe.servers`.

Example:

```bash
hs cloud enroll --gateway https://<gateway-host>
```

### `hs cloud status`

Show cloud connection status.

```text
hs cloud status [OPTIONS]
```

The command takes no arguments or flags of its own.

Prints the gateway URL and the device name, and checks the connection by refreshing the access token. It exits non-zero when the connection is broken, and prints `not enrolled` when no credentials exist.

Example:

```bash
hs cloud status
```

### `hs cloud token`

Print a fresh access token (for MCP server config, CI, etc.).

```text
hs cloud token [OPTIONS]
```

The command takes no arguments or flags of its own.

Prints only the access token on standard output, so it can be piped or captured; the lifetime goes to the status stream. It fails when the device is not enrolled.

Example:

```bash
hs cloud token
```

## `hs mcp`

Install or remove the home-still MCP server entry in Claude Desktop, Claude Code and OpenCode.

```text
hs mcp [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs mcp install` | Write the `home-still` and `myelin` servers into your clients' configuration. |
| `hs mcp uninstall` | Remove the `home-still` and `myelin` servers from your clients' configuration. |

These commands edit two MCP server entries in each client's configuration file: `home-still`, the tools server (`hs-mcp`), whose tools are listed in [MCP tools](mcp-tools.md), and `myelin`, the memory server, where an agent searches the library and keeps what it is asked to remember.

Example:

```bash
hs mcp --help
```

### `hs mcp install`

Write the `home-still` and `myelin` servers into your clients' configuration.

```text
hs mcp install [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--client <CLIENT>` | `all` | Target client: `desktop` (Claude Desktop), `code` (Claude Code), `opencode` (OpenCode) or `all`. |
| `--remote` | off | Reach both servers through the cloud gateway instead of on this machine. |
| `--gateway-url <GATEWAY_URL>` | none | Gateway URL for remote mode (reads from cloud config if omitted). |

Local mode writes two entries. `home-still` runs the `hs-mcp` binary as the client's child process and copies the variables from `~/.home-still/secrets.env` into the entry, so the client's file holds those values; when `hs-mcp` is not installed the command downloads it from GitHub. `myelin` is a URL entry for the memory server on this machine, `http://127.0.0.1:7463/mcp`. `--remote` writes two URL entries that point at the gateway instead, `<gateway>/mcp` and `<gateway>/memory/mcp`; it takes the gateway from `--gateway-url` or from the enrolled credentials.

Claude Desktop reads only commands from its file, so a URL entry is never written there: the command prints the URL to add as a connector in the app, and `--client desktop --remote` writes nothing and fails.

The entries are written to `claude_desktop_config.json` (Claude Desktop), `~/.claude.json` (Claude Code) and `~/.config/opencode/opencode.json` (OpenCode). Existing content is preserved, an entry of the same name is replaced, and the file is replaced atomically.

Example:

```bash
hs mcp install --client desktop
```

### `hs mcp uninstall`

Remove the `home-still` and `myelin` servers from your clients' configuration.

```text
hs mcp uninstall [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--client <CLIENT>` | `all` | Target client: `desktop` (Claude Desktop), `code` (Claude Code), `opencode` (OpenCode) or `all`. |

Removes the `home-still` entry from each selected client's configuration file.

Example:

```bash
hs mcp uninstall --client desktop
```

## `hs config`

View and manage configuration.

```text
hs config [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs config init` | Generate default config at ~/.home-still/config.yaml. |
| `hs config show` | Print the resolved configuration. |
| `hs config path` | Print the config file path. |

The configuration file is `~/.home-still/config.yaml`. Its sections and keys are listed in [configuration](configuration.md).

Example:

```bash
hs config --help
```

### `hs config init`

Generate default config at ~/.home-still/config.yaml.

```text
hs config init [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--force` | off | Overwrite existing config file. |

Prompts for an email address for Unpaywall, a CORE API key and an S3 secret key, each optional, and writes the default configuration. A non-empty S3 secret goes into `~/.home-still/secrets.env` (mode 0600). The command also creates `papers/manually_downloaded/`, `markdown/` and `catalog/` under the project directory. When the file exists, the command asks before overwriting it; `--force` and `--yes` overwrite without asking.

Example:

```bash
hs config init
```

### `hs config show`

Print the resolved configuration.

```text
hs config show [OPTIONS]
```

The command takes no arguments or flags of its own.

Prints the resolved `paper` configuration, as YAML or, with `--output json`, as JSON.

Example:

```bash
hs config show --output json
```

### `hs config path`

Print the config file path.

```text
hs config path [OPTIONS]
```

The command takes no arguments or flags of its own.

Prints the path of the configuration file.

Example:

```bash
hs config path
```

## `hs migrate`

Run data migrations.

```text
hs migrate [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs migrate sharding` | Move flat files into 2-char prefix subdirectories (papers/, markdown/, catalog/). |
| `hs migrate move-root-orphans` | Move sharded files that were written to the bucket root back under `papers/`. |
| `hs migrate personal-to-storage` | Copy the personal store into the corpus's storage under the `personal/` prefix. |
| `hs migrate quarantine-bad-content` | Find `papers/**/*.pdf` objects whose bytes are not a PDF, rename HTML to `.html`, and move other content to `papers/.quarantine/`. |
| `hs migrate drop-local-html` | Delete catalog rows whose `conversion.server` is `local-html`, together with their markdown and source HTML. |
| `hs migrate canonicalize-doi-stems` | Lowercase DOI-derived stems and merge case-collision duplicates onto one storage key. |
| `hs migrate dedupe-url-encoded` | Report documents stored under a URL-encoded stem and again under the decoded one. |

Migrations act immediately: none of them asks for confirmation. Every migration except `hs migrate sharding` and `hs migrate dedupe-url-encoded` has `--dry-run`, which prints the plan and changes nothing. `hs migrate dedupe-url-encoded` only reports.

Example:

```bash
hs migrate --help
```

### `hs migrate sharding`

Move flat files into 2-char prefix subdirectories (papers/, markdown/, catalog/).

```text
hs migrate sharding [OPTIONS]
```

The command takes no arguments or flags of its own.

Works on the local directories `paper.download_path`, `scribe.output_dir` and `scribe.catalog_dir`. It never overwrites: a flat file whose sharded twin exists is removed only when the content is identical, and is reported as a failure otherwise. The command has no `--dry-run`. It exits non-zero when any file failed or the run was interrupted.

Example:

```bash
hs migrate sharding
```

### `hs migrate move-root-orphans`

Move sharded files that were written to the bucket root back under `papers/`.

```text
hs migrate move-root-orphans [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Preview moves without writing anything. |
| `--limit <LIMIT>` | none | Cap on files to move in this run (for staged migration). |

The moved files were invisible to `hs status` while at the bucket root, because the status scan walks only `papers/`. `--limit` caps the files moved in one run, for a staged migration.

Example:

```bash
hs migrate move-root-orphans --dry-run
```

### `hs migrate personal-to-storage`

Copy the personal store into the corpus's storage under the `personal/` prefix.

```text
hs migrate personal-to-storage [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Preview without writing anything. |
| `--limit <LIMIT>` | none | Cap on files to move in this run (for staged migration). |

The personal store writes through the local file system, so it lives on one host and is not in the bucket or its backup, and the docs service cannot serve it. The command copies each source, sidecar and markdown file into the corpus's storage under `personal/` and `personal/markdown/`, keeping names. Local files are left in place. An object that already exists with the same size is skipped, so re-running is safe. It needs a `storage:` section and exits non-zero when any file failed. Without `--dry-run` it writes.

Example:

```bash
hs migrate personal-to-storage --dry-run
```

### `hs migrate quarantine-bad-content`

Find `papers/**/*.pdf` objects whose bytes are not a PDF, rename HTML to `.html`, and move other content to `papers/.quarantine/`.

```text
hs migrate quarantine-bad-content [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Preview without moving or stamping anything. |
| `--limit <LIMIT>` | none | Cap on files inspected this run. |

Inspects `papers/XX/*.pdf` for content-type mismatches such as a paywall page saved as `.pdf` or a truncated download. HTML is renamed to `.html`, so the HTML path converts it. Other non-PDF bytes move to `papers/.quarantine/XX/` and the catalog entry is stamped `conversion_failed`, so the watcher stops republishing them. Without `--dry-run` it moves and stamps immediately.

Example:

```bash
hs migrate quarantine-bad-content --dry-run --limit 1000
```

### `hs migrate drop-local-html`

Delete catalog rows whose `conversion.server` is `local-html`, together with their markdown and source HTML.

```text
hs migrate drop-local-html [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Preview without deleting anything. |

Removes the catalog rows, the markdown and the source HTML, so the pipeline treats those stems as new if they are downloaded again. Without `--dry-run` it deletes immediately.

Example:

```bash
hs migrate drop-local-html --dry-run
```

### `hs migrate canonicalize-doi-stems`

Lowercase DOI-derived stems and merge case-collision duplicates onto one storage key.

```text
hs migrate canonicalize-doi-stems [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Preview without moving, deleting, or purging anything. |
| `--limit <LIMIT>` | none | Cap on stems processed this run (for staged migration). |
| `--server <SERVER>` | none | Override distill server URL. |

DOIs are case-insensitive, but older stems were derived from the DOI verbatim, so one paper fetched as `10.48550/arXiv.X` and as `10.48550/arxiv.X` was stored twice and returned twice by search. The command handles live collisions (both spellings present: the larger markdown wins) and latent ones (only the mixed-case spelling present). It purges the dropped `doc_id`'s vectors through the distill server and re-indexes any stem whose canonical markdown changed. Non-DOI stems are never touched. Without `--dry-run` it moves, deletes and purges immediately.

Example:

```bash
hs migrate canonicalize-doi-stems --dry-run
```

### `hs migrate dedupe-url-encoded`

Report documents stored under a URL-encoded stem and again under the decoded one.

```text
hs migrate dedupe-url-encoded
```

This command has no options.

Lists the markdown of the `academic` store and finds each stem that contains a percent-encoded byte, such as `10.1%2Fabc`, whose decoded form is also a stem. It prints a JSON report with `pairs_found`, `would_delete_encoded_rows` and up to ten `samples`, each with the `encoded` and the `decoded` stem. The command changes nothing, so it has no `--dry-run`, and it exits with status 0 whenever it could read the storage.

Example:

```bash
hs migrate dedupe-url-encoded
```

## `hs pipeline`

Cross-service pipeline operations (rebuild from papers, etc.).

```text
hs pipeline [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs pipeline rebuild` | Delete the derived state (markdown, catalog entries, stored chunks and abstracts) and republish `papers.ingested` for every source under `papers/`. |
| `hs pipeline catch-up` | Republish `papers.ingested` for every source under `papers/` that has no markdown and no failure stamp. |
| `hs pipeline events-reset` | Delete the JetStream streams `PAPERS`, `SCRIBE` and `DISTILL` and recreate them from the `events` configuration. |
| `hs pipeline purge-skipped` | Delete HTML paywall and loading-stub artifacts that the indexer skipped as empty. |
| `hs pipeline purge-poisoned` | Delete documents that reached the store but whose text is an anti-bot or cookie-wall page. |
| `hs pipeline reap-phantoms` | Delete catalog entries that have no downloaded paper, no markdown and no embedding record. |
| `hs pipeline reconvert-failed` | Re-queue documents whose conversion was given up on. |
| `hs pipeline repair-catalog` | Report where the academic catalog and the storage disagree. |
| `hs pipeline backfill-titles` | Look up the title and other metadata of catalog entries that have a DOI and no title. |

Pipeline operations read `storage:`, `events:` and `distill.servers`. The destructive ones (`rebuild`, `purge-*`, `reap-phantoms`) have `--dry-run` and ask for confirmation unless `--yes` is given. `--yes` on these commands is a local, long-form flag: `-y` is not accepted.

Example:

```bash
hs pipeline --help
```

### `hs pipeline rebuild`

Delete the derived state (markdown, catalog entries, stored chunks and abstracts) and republish `papers.ingested` for every source under `papers/`.

```text
hs pipeline rebuild [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Count what would be deleted / republished without touching anything. |
| `--yes` | off | Skip the interactive prompts. Requires `--confirm rebuild-from-papers`. Long form only: `-y` is not accepted here. |
| `--confirm <CONFIRM>` | none | Confirmation token. Must equal `rebuild-from-papers` when `--yes` is set; otherwise the interactive prompt asks for it. |

Prints an inventory of what it would delete and republish, and refuses to continue when there are no sources under `papers/` or the first source cannot be read. `--dry-run` stops there. Otherwise it asks `Proceed?` (default no) and then asks you to type `rebuild-from-papers`; a different answer aborts with no change. In a script, pass both `--yes` and `--confirm rebuild-from-papers`; `--yes` without the matching token is an error. The papers themselves are never touched. Rebuilding takes hours of scribe work; `hs status` shows the progress. Every document is deleted from the `academic` store, with its reference lists, and from the `abstracts` store.

Example:

```bash
hs pipeline rebuild --dry-run
```

### `hs pipeline catch-up`

Republish `papers.ingested` for every source under `papers/` that has no markdown and no failure stamp.

```text
hs pipeline catch-up [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be republished without touching anything. |

Lists the catalog's failure stamps first. Sources whose conversion failed are skipped and counted, and the output names `hs pipeline reconvert-failed` as the way to re-queue them. The command never deletes anything. It is meant for bringing a new scribe worker online mid-rebuild and for re-queuing papers whose events were lost. It exits non-zero when any publish failed or the run was interrupted.

Example:

```bash
hs pipeline catch-up --dry-run
```

### `hs pipeline events-reset`

Delete the JetStream streams `PAPERS`, `SCRIBE` and `DISTILL` and recreate them from the `events` configuration.

```text
hs pipeline events-reset [OPTIONS]
```

The command takes no arguments or flags of its own.

It exists for a stream or consumer that is stuck with a configuration that cannot be updated in place, for example a different stream retention. All queued and in-flight events are discarded. The command needs `events.backend: nats`, has no dry run and no confirmation prompt. `hs pipeline catch-up` queues the unconverted papers again.

Example:

```bash
hs pipeline events-reset
```

### `hs pipeline purge-skipped`

Delete HTML paywall and loading-stub artifacts that the indexer skipped as empty.

```text
hs pipeline purge-skipped [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be deleted without touching anything. |
| `--yes` | off | Skip the interactive confirmation prompt. |

Selects catalog entries stamped `embedding_skip.reason = zero_chunks_or_empty` that the HTML parser produced (for example PMC "Preparing to download" interstitials). For each it deletes the source `.html`, the `.md` and the catalog `.yaml`. `--dry-run` lists the selection and stops. Without `--yes` the command asks `Delete N HTML stubs?` (default no).

Example:

```bash
hs pipeline purge-skipped --dry-run
```

### `hs pipeline purge-poisoned`

Delete documents that reached the store but whose text is an anti-bot or cookie-wall page.

```text
hs pipeline purge-poisoned [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be deleted without touching anything. |
| `--yes` | off | Skip the interactive confirmation prompt. |

Finds markdown stubs that passed the chunk-quality floor and reached the store as one-chunk documents, but whose text matches a known anti-bot or cookie-wall signature. For each it deletes the stored chunks and the abstract, the markdown, the source PDF or HTML, and the catalog `.yaml`. `--dry-run` lists the selection and stops. Without `--yes` the command asks before deleting (default no).

Example:

```bash
hs pipeline purge-poisoned --dry-run
```

### `hs pipeline reap-phantoms`

Delete catalog entries that have no downloaded paper, no markdown and no embedding record.

```text
hs pipeline reap-phantoms [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be deleted without touching anything. |
| `--yes` | off | Skip the interactive confirmation prompt. |

Phantom entries accumulate from papers that were cataloged before a download that then failed or was abandoned. They inflate the drift counter but break nothing. The command deletes only the orphan catalog `.yaml`. `--dry-run` lists the selection and stops. Without `--yes` the command asks before deleting (default no).

Example:

```bash
hs pipeline reap-phantoms --dry-run
```

### `hs pipeline reconvert-failed`

Re-queue documents whose conversion was given up on.

```text
hs pipeline reconvert-failed [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be reconverted without touching anything. |
| `--yes` | off | Skip the interactive confirmation prompt. |
| `--limit <LIMIT>` | none | Re-queue at most this many documents. Without it, all selected documents. |
| `--reason <REASON>` | none | Re-queue exactly the rows stamped with this reason, whatever its kind, instead of every retryable one. Repeat the flag for several reasons. |

Selects the catalog rows stamped `conversion_failed`. By default it selects the retryable ones (a backend failed on the document); `--reason` selects exactly the named reasons, of any kind. The output groups the selected and the remaining rows by reason. For the selected rows it clears the failure stamp and republishes `papers.ingested`; nothing is deleted. `--dry-run` prints the inventory and stops. Without `--yes` the command asks before republishing (default no). The reasons are listed in [status dashboard](status-dashboard.md#conversion-failure-reasons).

Example:

```bash
hs pipeline reconvert-failed --dry-run --limit 25
```

### `hs pipeline repair-catalog`

Report where the academic catalog and the storage disagree.

```text
hs pipeline repair-catalog [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--limit <LIMIT>` | none | Cap each `would_*` count at this many entries. Without it, no cap. |

Reads every source, every markdown object and every catalog entry of the `academic` store once, then looks for seven kinds of disagreement in that snapshot and prints one JSON report with a count and up to ten example stems for each: `disk_no_catalog` (a source with no catalog entry), `catalog_no_markdown` (an entry that says the document was converted, with no markdown), `catalog_no_source` (an entry with neither a source nor markdown), `flag_drift` (a stage flag is missing but storage shows the stage done), `flag_drift_resync` (timestamps that still carry the stamp of one earlier batch), `md_path_drift` (a recorded markdown path that is not where the markdown is) and `stuck_convert` (a source that was never converted and has no failure stamp). The command changes nothing. It exits with status 0 whenever it could read the storage, whatever it found. `hs pipeline reap-phantoms` deletes the entries that `catalog_no_source` lists.

Example:

```bash
hs pipeline repair-catalog --limit 100
```

### `hs pipeline backfill-titles`

Look up the title and other metadata of catalog entries that have a DOI and no title.

```text
hs pipeline backfill-titles [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Report what would be looked up without contacting a provider or writing anything. |
| `--limit <LIMIT>` | none | Look up at most this many entries. Without it, all of them. |

An entry is a candidate when it has a DOI and its title is missing or blank. The command asks the paper providers for each candidate by DOI. An entry whose lookup returns a title gets the title, plus the authors, publication date, abstract and citation count it lacks; a field it already has is kept. The entry is read again just before it is written, so a stamp written meanwhile is not lost, and it is marked as repaired. The JSON report has `candidates`, `backfilled`, `no_metadata` (the providers know nothing under that DOI), `errors` (one line for each entry whose lookup or write failed), up to ten `samples` and `dry_run: false`. With `--dry-run` it contacts no provider and writes nothing, and the report has `candidates`, `would_backfill`, `samples` and `dry_run: true`. The command exits with an error when `errors` is not empty, after printing the report. Provider settings are read from the paper configuration.

Example:

```bash
hs pipeline backfill-titles --dry-run --limit 50
```

## `hs openalex`

Local OpenAlex catalog (DuckDB) — bulk-load entities, query, build FTS.

```text
hs openalex [OPTIONS] <COMMAND>
```

| Command | Description |
|---|---|
| `hs openalex load` | Load a "simple" entity (concepts, topics, authors, sources, …). Partitions are walked newest-first and the newest copy of each ID wins. |
| `hs openalex load-works` | Load works partitions with streaming pre-dedupe; the run fails on the first failed partition and resumes when re-run. |
| `hs openalex status` | Print row counts per table and any failed / lossy ingest-log entries. |
| `hs openalex query` | Run an ad-hoc SELECT (read-only connection) and print row tuples. |
| `hs openalex build-fts` | Build the BM25 FTS index over works.title + abstract_text (needs the extension: run `install-fts` first; this never downloads it). |
| `hs openalex build-indexes` | Build the post-load secondary indexes. |
| `hs openalex install-fts` | Download and load the DuckDB `fts` extension that `build-fts` needs. Needs network access once per host and DuckDB version; opens no database. |

The commands read `openalex.db_path` and `openalex.snapshot_dir` from the configuration. The steps, in order: `load` and `load-works`, then `build-indexes`, `install-fts`, `build-fts`. The writers (`load`, `load-works`, `build-indexes`, `build-fts`) take the database file exclusively and are refused on Windows; `status` and `query` open it read-only. The `openalex_*` MCP tools appear after the corpus is built and `hs-mcp` restarts.

Example:

```bash
hs openalex --help
```

### `hs openalex load`

Load a "simple" entity (concepts, topics, authors, sources, …). Partitions are walked newest-first and the newest copy of each ID wins.

```text
hs openalex load [OPTIONS] <ENTITY>
```

| Name | Default | Meaning |
|---|---|---|
| `<ENTITY>` | required | Entity to load: `concepts`, `topics`, `domains`, `fields`, `subfields`, `sources`, `institutions`, `funders`, `publishers` or `authors`. |
| `--max-parse-errors <MAX_PARSE_ERRORS>` | none | `authors` only. Fail a partition once more than this many of its records are unusable (unparseable, or without an `A<digits>` id). Default 100. The other entities fail on the first malformed line and take no limit. |

Walks the snapshot's partitions newest-first; the newest copy of each id wins.

Example:

```bash
hs openalex load concepts
```

### `hs openalex load-works`

Load works partitions with streaming pre-dedupe; the run fails on the first failed partition and resumes when re-run.

```text
hs openalex load-works [OPTIONS]
```

| Name | Default | Meaning |
|---|---|---|
| `--partition <PARTITION>` | none | . |
| `--max-parse-errors <MAX_PARSE_ERRORS>` | none | Fail a partition once more than this many of its records are unusable (unparseable, or without a `W<digits>` id). Records within the limit are skipped and counted in `_ingest_log.parse_errors`. |

Parses the partitions, fans the rows out to five appenders, and gates each row through an in-memory set of work ids, so the live tables keep their primary keys without `ON CONFLICT`. The set is checkpointed beside the database as `seen_set.bin`. With `--partition` the set is rebuilt from the live `works` table, so existing rows still dedupe.

Example:

```bash
hs openalex load-works --partition updated_date=2024-01-01
```

### `hs openalex status`

Print row counts per table and any failed / lossy ingest-log entries.

```text
hs openalex status [OPTIONS]
```

The command takes no arguments or flags of its own.

Opens the database read-only.

Example:

```bash
hs openalex status
```

### `hs openalex query`

Run an ad-hoc SELECT (read-only connection) and print row tuples.

```text
hs openalex query [OPTIONS] <SQL>
```

| Name | Default | Meaning |
|---|---|---|
| `<SQL>` | required | A single read-only `SELECT` statement. |

Opens the database read-only and prints the rows.

Example:

```bash
hs openalex query "SELECT COUNT(*) FROM works"
```

### `hs openalex build-fts`

Build the BM25 FTS index over works.title + abstract_text (needs the extension: run `install-fts` first; this never downloads it).

```text
hs openalex build-fts [OPTIONS]
```

The command takes no arguments or flags of its own.

Only loads the `fts` extension; run `hs openalex install-fts` first. The command never downloads it.

Example:

```bash
hs openalex build-fts
```

### `hs openalex build-indexes`

Build the post-load secondary indexes.

```text
hs openalex build-indexes [OPTIONS]
```

The command takes no arguments or flags of its own.

Example:

```bash
hs openalex build-indexes
```

### `hs openalex install-fts`

Download and load the DuckDB `fts` extension that `build-fts` needs. Needs network access once per host and DuckDB version; opens no database.

```text
hs openalex install-fts [OPTIONS]
```

The command takes no arguments or flags of its own.

Needs network access once per host and DuckDB version, and opens no database.

Example:

```bash
hs openalex install-fts
```
