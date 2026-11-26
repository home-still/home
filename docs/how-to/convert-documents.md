# Convert documents

Run a scribe server, convert documents by hand or automatically, and recover
conversions that failed or never ran. How conversion works is explained in
[conversion](../explanation/conversion.md).

## Before you start

- A scribe host needs `hs-scribe-server` in `~/.local/bin/`, libpdfium in
  `~/.local/lib/` (or `~/.home-still/dyld-libs/`, or the system library
  path), the layout and table models and the word OCR's three files in
  `~/.home-still/models/`, a VLM backend
  (Ollama with `glm-ocr` by default), and `HS_BACKEND_TOKEN` in
  `~/.home-still/secrets.env`. [Build your first research
  library](../tutorials/first-library.md#5-set-up-conversion) walks through
  each.
- Every host that sends documents to a scribe server lists it in
  `scribe.servers` and has the same `HS_BACKEND_TOKEN`. See
  [`scribe`](../reference/configuration.md#scribe).
- The watcher, the inbox and the recovery commands need the `events:`
  section and a running NATS broker.

## Run the scribe server

1. Run it in the foreground to check it starts and to watch its log:

   ```sh
   hs serve scribe
   ```

   Stop it with Ctrl+C.

2. Install it as a service that starts at boot and restarts on failure:

   ```sh
   hs serve scribe --install
   ```

   The service reads `~/.home-still/secrets.env`. Remove it with
   `hs serve scribe --uninstall`.

3. To run a second server on the same host, for example with the olmocr
   converter, give it another port and its own settings (see
   [`scribe_server`](../reference/configuration.md#scribe_server)):

   ```sh
   HS_SCRIBE_CONVERTER=olmocr hs serve scribe --port 7435
   ```

   `--install` registers one scribe service per host, so supervise a second
   server yourself.

## Run the scribe server in a container

A multi-architecture image is published with every release. It bundles
libpdfium and reads the models from `/models`:

```sh
docker run -d -p 7433:7433 --add-host=host.docker.internal:host-gateway \
  -v ~/.home-still/models:/models:ro \
  -e HS_BACKEND_TOKEN="$HS_BACKEND_TOKEN" \
  -e HS_SCRIBE_OLLAMA_URL=http://host.docker.internal:11434 \
  ghcr.io/home-still/hs-scribe-server:latest
```

On macOS, run the server natively instead: Docker there cannot use the GPU,
and a native server next to a native Ollama uses Metal.

## Convert one file

```sh
hs scribe convert paper.pdf --out paper.md
```

Without `--out`, the markdown goes to standard output. `--server` sends the
file to one server instead of the configured list:

```sh
hs scribe convert paper.pdf --server http://<scribe-host>:7433 --out paper.md
```

## Convert everything that is downloaded

The scribe watcher takes each `papers.ingested` event, sends the document to
the scribe tiers in `scribe.servers`, and stores the markdown and catalog
entry in the store the document's key belongs to. For a PDF it then has a
scribe server build the word boxes, which record where each word sits on its
page, and stores those too. A Markdown or plain-text source needs no
server: the watcher stores its text as the markdown.

1. Install it as a user service:

   ```sh
   hs serve scribe-watch --install
   ```

   Or run it in the foreground:

   ```sh
   hs scribe watch-events
   ```

2. Run one watcher per cluster. Several watchers share the work, but one is
   enough: the scribe servers do the converting.

## Process files you add by hand

The inbox moves files from `papers/manually_downloaded/` into the library and
announces them, polling every `scribe.inbox_poll_interval_secs` (30 by
default). Run it on one host that can write to the storage.

1. Install it as a user service:

   ```sh
   hs scribe inbox install
   ```

2. Check it:

   ```sh
   hs scribe inbox status
   ```

To process the inbox once, run `hs scribe inbox sweep`. A file whose name
cannot become a stem is moved to `corrupted/` with the reason logged.

## Convert one document again

```sh
hs scribe reconvert 10.48550_arxiv.2312.10997
```

This clears the document's conversion and failure stamps and announces it
again, so the watcher converts it from scratch.

## Build the word boxes that are missing

The watcher builds a PDF's word boxes after its conversion. A PDF converted
before the watcher did that, or whose build kept failing, has none.

1. See how many are missing and how many builds are queued:

   ```sh
   hs scribe boxes status
   ```

2. When `queued` and `building` are 0, queue the missing ones:

   ```sh
   hs scribe boxes catch-up
   ```

   `--dry-run` prints the keys instead of queueing them, and
   `--store personal` limits the run to one store.

3. Run `hs scribe boxes status` again until `missing` is 0. The watcher's log
   names each PDF it could not build word boxes for, and why.

## Queue documents that were never converted

`hs status` shows a non-zero Stalled count when sources have no markdown, no
failure stamp, and no queued event.

1. See what would be queued:

   ```sh
   hs pipeline catch-up --dry-run
   ```

   It lists the documents it would announce, and counts the failed
   conversions it skips.

2. Queue them:

   ```sh
   hs pipeline catch-up
   ```

## Retry failed conversions

`hs status` shows the retryable failures on the "Failed, retry" row. Fix
their cause first (add a tier, free the GPU, upgrade the converter), then:

1. See what would be re-queued and what would be left:

   ```sh
   hs pipeline reconvert-failed --dry-run
   ```

2. Re-queue every retryable failure, or a few first:

   ```sh
   hs pipeline reconvert-failed --limit 25
   hs pipeline reconvert-failed
   ```

   Each asks for confirmation first; `--yes` skips the question.

3. To re-queue one specific reason, of any kind, name it (repeatable):

   ```sh
   hs pipeline reconvert-failed --reason gapped_conversion --dry-run
   hs pipeline reconvert-failed --reason pdf_parse_error
   ```

The reasons and their kinds are in
[conversion failure reasons](../reference/status-dashboard.md#conversion-failure-reasons).

## Check it worked

```sh
HS_MCP_URL=http://localhost:7445/mcp hs status
```

The Markdown count rises, In-flight shows conversions running, and Queued
falls. The scribe rows under Services show each server as healthy; a server
that cannot take work explains why there (see
[troubleshoot](troubleshoot.md)).
