# Changelog

Changes that need action when upgrading. Binaries and full notes: GitHub Releases.

## v0.0.1-rc.394

- The scribe server answers `POST /scribe/boxes`: the body is a PDF, the
  response its word-box sidecar (every word of every page with its box),
  read from the text layer, or by a word OCR on the CPU for a page that has
  none. **Each host that runs a scribe server needs three more model files
  in `~/.home-still/models/` before it can read a scanned page**:
  `pp-ocrv5_mobile_det.onnx`, `en_pp-ocrv5_mobile_rec.onnx` and
  `ppocrv5_en_dict.txt`, from
  `https://github.com/GreatV/oar-ocr/releases/download/v0.3.0/<file>`.
  Without them a PDF with a text layer still gets its sidecar and a scanned
  one is answered `500` naming the missing file. The keys
  `scribe_server.ocr_detection_model_path`, `ocr_recognition_model_path` and
  `ocr_dictionary_path` name other files.
- `hs scribe watch-events` reads a second queue, `scribe.boxes`, and builds
  the word boxes of each PDF it names through `/scribe/boxes`. The request is
  published when a PDF's markdown is stored and by `hs personal add` for a
  PDF. **Upgrade the scribe servers before the watcher**: a server without
  `/scribe/boxes` answers 404, and a request no server can answer is retried
  and then dropped after `events.nats.max_deliver` deliveries. **`hs personal
  add` and `personal_add` now need the `events:` section and a reachable
  broker for a PDF**, and refuse the PDF before converting it without them.
  PDFs converted before this release have no word boxes: once the upgraded
  watcher is running, run `hs scribe boxes catch-up` and watch
  `hs scribe boxes status` until `missing`, `queued` and `building` are 0.
- Document links are signed under a new rule that also covers the words a
  link highlights. **A link or short link minted before the upgrade is
  refused with `403` by an upgraded docs service, and a link minted by an
  upgraded host is refused by a docs service that is not**: upgrade the docs
  service and every host that mints links (`hs docs link`, the MCP server)
  together. Links last an hour unless `docs.link_ttl_secs` says otherwise, so
  nothing has to be minted again by hand.
- `hs docs link --collection` and `doc_link`'s `collection` are checked
  against the list of stores (`academic`, `personal`, `web`, `syntheses`).
  **A name that is not a store is now refused when the link is minted**; it
  used to be signed and then answered with a 404.
- `personal.storage_dir` is checked at startup by every command that reads
  it, the docs service and the MCP server: it must be a relative path
  without `.` or `..` parts and must not overlap another store's prefix
  (`papers`, `markdown`, `catalog`, `boxes`, `web`, `syntheses`). The docs
  service now serves personal documents from this prefix; it used to read
  `personal/` whatever the setting said.
- `doc_link` takes `short: true`, and `hs docs link --short` mints a
  12-character short link (`<base>/s/<code>`) that redirects to the signed
  URL. The code is a record in the storage backend and expires with the link
  behind it; `hs docs prune` deletes the expired ones. Search hits never mint
  one.
- The docs service answers `/s/{code}` with a `302` to the signed link, and
  the gateway forwards `/s/*` unauthenticated for the same reason it forwards
  `/docs/*`: a browser following the redirect has no bearer token.
- `scribe_queue` is a new MCP tool that publishes `papers.ingested` and
  returns at once. `scribe_convert` is unchanged in behaviour but now says in
  its description that it blocks for the whole conversion, bounded by the
  3600 s timeout ceiling.
- `distill_exists` names its argument `stem`, like every other stem-taking
  tool. **A client sending `doc_id` is now refused with `missing field`**; the
  response also echoes `stem` rather than `doc_id`.
- `distill_reconcile` reports `requested_apply_ignored` in its response, which
  is what its description had been promising.

## v0.0.1-rc.388

- `hs upgrade` leaves a compose service running when its image did not
  change and it is healthy, instead of taking every managed container down
  and up again.

## v0.0.1-rc.387

- `hs pipeline catch-up` skips documents stamped `conversion_failed`, reports
  how many it skipped by kind, and points at `hs pipeline reconvert-failed`.
  It no longer re-queues failed conversions ahead of new downloads.
- The dashboard's Stalled row counts only documents with no markdown, no
  failure stamp and no queued event.

## v0.0.1-rc.386

- The dashboard's "Failed, retry" row replaces "Corrupted PDFs". It counts
  retryable failures and names the recovery command; bad sources are counted
  beside it and are the only failures left out of the Markdown progress.
- `hs pipeline reconvert-failed` re-queues every retryable failure by
  default; `--reason <reason>` (repeatable) selects exactly the named
  reasons, of any kind.
- MCP: `system_status.pipeline.failed_conversions` (`retryable`,
  `bad_source`, `unclassified`, and the reasons with their kind) replaces
  `corrupted_pdfs`, and `catalog_list` rows carry
  `conversion_failed: {reason, kind}` instead of `corrupted`. Upgrade `hs`
  and the MCP server together: an older server leaves the row unknown.

## v0.0.1-rc.385

- The streaming repetition detector fires only on a consecutive loop: at
  least 3 back-to-back copies of a unit of up to 128 words, spanning at least
  32 words. Affiliation lists, citation runs and repeated key terms no longer
  abort a region, so they no longer reject whole conversions as
  `gapped_conversion`. Re-queue the documents this dropped with
  `hs pipeline reconvert-failed --reason gapped_conversion` after every scribe
  host runs this release.

## v0.0.1-rc.384

- The olmocr converter keeps the PDF's pages, so its documents get a page
  map, every passage a `page`, and every source link `#page=N`. Documents
  olmocr converted earlier are one flat page, and their hits carry no `page`,
  until they are converted again (`hs scribe reconvert <stem>`).

## Robustness release

Do these **before** restarting any service on the new binaries; each item names the symptom if it is skipped. Hosts are placeholders (`<gateway-host>`, `<gpu-host>`, ...).

### 1. Provision `HS_BACKEND_TOKEN` (and understand its blast radius)

* Generate once: `openssl rand -hex 32`. Put the **same** value in `~/.home-still/secrets.env` (`HS_BACKEND_TOKEN=<64-hex-chars>`, mode 0600) on the gateway host, on every backend host (`hs-mcp --serve`, distill, scribe) and on every host that runs `hs`. Minimum 32 bytes of visible ASCII; a shorter or malformed value stops the process with an error naming the variable.
* Without it: `hs-mcp --serve`, `hs-distill-server`, `hs-scribe-server` and the gateway refuse to start; clients get 401 from backends.
* **What this secret is.** One static bearer for the whole cluster, not a per-service or per-user credential. It is sent **in cleartext over plain HTTP on the LAN**, and `AuthedHttp::plain` attaches it to *whatever URL is configured* under `scribe.servers` / `distill.servers` (and through `HTTP(S)_PROXY` if one is set). Consequences: keep those URLs on the trusted LAN and double-check them for typos; a LAN sniffer, or the compromise of **any one** client host, unlocks **every** backend and bypasses the gateway's scopes (including the `personal_*` tools). Treat `secrets.env` on every host as a cluster root credential, and rotate by regenerating and redeploying to all hosts at once.
* Follow-up options (not implemented): per-service tokens (a leaked scribe token no longer opens distill/mcp), and TLS between gateway/clients and backends so the token is not on the wire in clear. Recommendation: TLS first, then per-service tokens.
* `hs-scribe-server` requires the token: every route except `GET /health` and `GET /readiness` answers 401 without `Authorization: Bearer <token>` (paths no route serves included). Provision it on **every** scribe host — the macOS launchd one included (`hs serve scribe --install` units read `secrets.env`; nothing generates a scribe compose file or provisions the token for you) — and on every host that runs a scribe client (`hs`, `hs-mcp`), before upgrading any of them: a client without it gets 401 on every convert (retried as transient, loudly).

### 2. Gateway

* Re-enrollment is required: old access and refresh tokens are rejected. Order: upgrade `hs-gateway` **and** `hs` on the gateway host together, make sure the gateway unit runs the new binary, `hs cloud invite --name <device>` per device, then `hs cloud enroll --gateway https://<public-host>` on each device.
* The unit's `ExecStart` needs `--gateway-url https://<public-host>` (startup fails without it).
* `cloud.gateway.routes` must list **every** backend (`scribe`, `distill`, `mcp`, `docs`; a single URL or a list of `http(s)://host[:port]` URLs). Other keys, paths in URLs or typos are startup errors, and so is a service without a route (the gateway names the key to add). The dynamic registry and `hs server` are gone, and `hs serve ...` no longer registers with the gateway.
* Remove the stale `cloud.role` / `cloud.gateway_url` keys; nothing reads them.

### 3. Config that is now required or validated

* `events:` with an explicit `backend` (`nats` or `noop`) is required by the watchers, `hs scribe inbox`, `hs pipeline`, `hs migrate`, `hs paper download` and `hs-mcp`. Watchers need `backend: nats`. `hs-mcp` validates the section at start (a missing section, an invalid key, an unset `token_env`/`password_env` variable or a missing credentials/certificate file stops it) but connects to the broker on first publish: with the broker down it still starts and serves every read-only tool, and `paper_download` / `scribe_convert` answer with a tool error before doing any work.
* Watchers (`hs scribe watch-events`, `hs distill watch-events`) that lose their subscription (consumer deleted, connection dropped, missed heartbeats) wait up to `events.nats.drain_timeout_secs` (default 120) for handlers already running, log an ERROR naming any they abandon (their events are redelivered after `ack_wait`), and exit non-zero so the supervisor restarts them. A watcher no longer deletes and recreates the durable consumer at start: it reuses it and, if its settings differ from the configured ones, updates them in place. Two watchers sharing one durable name therefore load-balance instead of tearing each other's consumer down.
* `scribe.servers` on every host that runs a scribe client command; `distill.servers` on every host that runs distill commands.
* Scribe **server** settings live under `scribe_server:` in `~/.home-still/config.yaml` (or `HS_SCRIBE_*` env, which wins). The old `~/.config/home-still/config.yaml` is no longer read. Invalid values stop `hs-scribe-server` instead of starting on defaults.
* A malformed `storage:`, `events:`, `logs:`, `home:`, `scribe:`, `distill:`, `scribe_server:` or `distill_server:` section, an unreadable `secrets.env`, or an invalid `HS_*` / `HOME_STILL_*` value now stops the binary (exit 1, or 2 for logging) with the key named.
* Unknown keys: inside `storage:` (including `storage.local` and `storage.s3`) and `home:` they are **errors** naming the key and the valid ones (these decide where data lives: `storage.root` instead of `storage.local.root` used to run on the default root without a word). Inside `scribe`, `scribe_server`, `distill`, `distill_server`, `paper`, `personal` and `logs` they are **warnings**, logged once at start with the section, the key and the valid keys; so is an `HS_SCRIBE_*` / `HS_DISTILL_*` variable that sets nothing (the generic scribe "timeout secs" variable the README used to list never mapped to a field; use `HS_SCRIBE_VLM_IDLE_TIMEOUT_SECS`, `HS_SCRIBE_OLLAMA_REQUEST_TIMEOUT_SECS` or `HS_SCRIBE_CONVERT_DEADLINE_SECS`). `cloud.gateway` keys are not checked.
* **Operator decision:** with no `storage:` section the local storage root defaults to `~/home-still`. If `home.project_dir` is set and names a different directory, the config is **refused** (an error naming both paths and the exact `storage:\n  backend: local\n  local:\n    root: <path>` section to add), so objects never silently split from the rest of the project data. An explicit `storage:` section is never checked.
* Paths: `storage.local.root`, `home.project_dir` and `home.log_dir` must be absolute after `~/` expansion; a relative value is an error naming the key (use an absolute path or `~/...`).
* S3 storage: every request has a fixed 15-minute total timeout (no config key); connect timeout and retries are object_store's defaults.
* `RUST_LOG`, when set, overrides the stderr log level (not under `--quiet`); an unparsable value is a startup error.
* Paper config: a provider API key with a plain-`http` non-loopback `base_url` refuses to load (e.g. `openalex.api_key` with `http://api.openalex.org`: use https).
* **CORE is optional.** Without `paper.providers.core.api_key`, CORE is left out of `--provider all` (one info line at start: `CORE provider disabled: no paper.providers.core.api_key`) instead of failing every aggregate search; `--provider core` still reports the missing key. (`paper.download.core_api_key` is not read by anything; move it to `paper.providers.core.api_key`.)
* **`hs paper search --sort date|citations` is a strict ordering** (newest / most cited first, missing values last) after the relevance floors, no longer a blend with relevance.
* **`hs personal add` refuses a file whose content is already stored** (sha256 of any existing sidecar), naming the existing stem: `duplicate content (already ingested as stem '<stem>'); rerun with --force to replace it`. With `--force` (or the `personal_add` `force` parameter) the new document is ingested first, then every older stem with the same content (document, markdown, sidecar, vectors) is removed.

* **Rollout order: binaries first, then config.** `events` and `events.nats` reject unknown keys, so an older binary refuses to start on a config that already has `events.nats.drain_timeout_secs` (or any other key added in this release). Upgrade every host's binaries, then add new keys.
* **Watcher restarts and the durable consumer.** A watcher no longer deletes its durable consumer at start, so messages the previous process had pulled but not acknowledged are redelivered only after `events.nats.ack_wait_secs` (default 7200 s), up to `max_ack_pending` (32) per consumer, after each restart (`hs upgrade`, a fatal exit). To get them back sooner, delete the consumer while the watcher is stopped (`nats consumer rm <STREAM> <durable>`); it is recreated at the next start. Settings JetStream cannot change on an existing consumer (`ack_policy`, `deliver_policy`) make the watcher refuse to start with an error giving the live and wanted values and the remedy: restore the configuration, or delete the consumer (only when it has no pending or ack-pending messages, check with `nats consumer info`). The watcher never deletes a consumer itself. Mutable settings (`ack_wait`, `max_deliver`, `max_ack_pending`, `filter_subject`, `backoff`) are updated in place with a WARN.

### 4. Scribe hosts

* **libpdfium is required on every host that runs `hs-scribe-server` or a watcher (`hs scribe watch-events`), on every converter — olmocr hosts included** (they counted pages without it before): pdfium is the one PDF parser and counts the pages of every PDF before it is dispatched or converted. Both refuse to start when it cannot be bound, naming where they looked (`~/.local/lib`, `~/.home-still/dyld-libs`, then the system library path — never the working directory; macOS: `libpdfium.dylib`, see [the cluster tutorial](docs/tutorials/home-cloud-cluster.md#17-add-a-second-scribe-worker); container images bundle it). `hs scribe convert` and the `scribe_convert` MCP tool need it too, but fail per call instead. Check with `ldconfig -p | grep pdfium` (Linux) before upgrading. Legacy per-region hosts will not start without both ONNX models; check `/health` shows both loaded before upgrading. An olmocr host will not start unless `olmocr_bin` (`HS_SCRIBE_OLMOCR_BIN`) is an executable file or on `PATH`.
* A PDF pdfium cannot open, or that has no pages or more than 65 535, is refused for good (`pdf_parse_error`); a counter stuck behind a hostile document makes the callers behind it retry instead. A PDF the old parser rejected but pdfium renders is now converted.
* **pdfium faults end the process.** A panic that unwinds through a live pdfium object poisons pdfium-render's process-wide lock, and a native pdfium call that never returns cannot be killed from inside the process. Both turn `/health` red (`status: pdfium_fault`, 503) and, after a 10 s grace, exit with status 70 so the supervisor restarts the unit (keep `Restart=always`/`RestartSec`). A panic during conversion is a typed permanent failure (`conversion_panicked`) on the wire, never a verdict about the PDF.
* **Accepted residual: a PDF that keeps pdfium busy longer than its budget plus the grace period restarts the whole process.** The page count has a 60 s budget, each Legacy render/open call 120 s (`pdfium::guarded_call`), then a 10 s grace; past that the process exits 70 (`_exit`, so a wedged thread cannot block shutdown) and the supervisor restarts it, ending every conversion in flight on that host (up to `vlm_concurrency` of them). In-process recovery is impossible: pdfium holds a process-wide lock for the life of a call, a native call cannot be cancelled or killed from another thread, and a thread stuck inside it cannot be abandoned while the lock stays held. The only alternative is counting/rendering in a killable child process, which is deliberately not built. The document itself is refused for good (`pdf_parse_error`) on the count path, so it does not loop. The same restart follows a panic that poisons a layout/table ONNX slot (`DetectorPool`).
* **What happens to those in-flight conversions.** A Legacy host serializes pdfium work (stage 1: open, render, layout) across conversions through pdfium-render's process-wide lock, while the VLM stage runs outside it, so `vlm_concurrency` > 1 is intended and sizes admission, the VLM semaphore and `/readiness` `vlm_slots_total`. When the exit-70 fires, each caller's HTTP connection drops. A caller using `hs scribe watch-events` (a separate process) sees an error with no typed `ConvertFailure`, which `classify` treats as `Transient`, and NAKs the event with `NAK_BACKOFF`; the event is acked only after a successful convert and upload, so JetStream redelivers it (bounded by the consumer's `max_deliver`). Caveats: a document that wedges a *render* (the page count passed) is redelivered and can wedge the server again until `max_deliver` retires it; and a watcher that dies together with the server (same host crash) leaves its events un-acked until `ack_wait` expires (see the next bullet). Callers that are not event watchers (`hs scribe convert`, MCP `scribe_convert`) just get the error and nothing retries for them.
* **Events in flight at a restart.** On an orderly stop (broker/stream ended, delivery error, a rejected token) the watcher hands back every event it pulled but never started (NAK, immediate redelivery, bounded to 256 events / 5 s). After a crash or the exit-70 above those events stay invisible to the broker until `ack_wait` (default 2 h) expires.
* **`ack_wait` and the in-progress heartbeat.** While a handler runs, both watchers (`hs scribe watch-events`, `hs distill watch-events`) send a JetStream in-progress heartbeat for the event every `events.nats.ack_wait_secs / 3` (7200 s default → every 40 min), which resets the broker's `ack_wait` timer. `ack_wait_secs` therefore no longer has to exceed a whole conversion (the full scribe tier chain); it now sets how long an event stays invisible after a watcher crash or exit-70 (the case above), so a smaller value gets those events back sooner. A handler that finishes before the first heartbeat sends none; a failed heartbeat is logged at WARN and does not fail the handler. The heartbeat stops when the handler finishes (it is not a separate task). Changing `ack_wait_secs` is a mutable consumer setting: the watcher updates it in place at start.
* **`ack_wait` must still cover the queue.** Only an event whose handler is running is heartbeated. An event the watcher has pulled but that waits for a free handler slot (up to `events.nats.max_ack_pending` events per consumer, default 32) is not: its `ack_wait` timer runs while it waits behind the running conversions, and if the wait exceeds `ack_wait_secs` the broker redelivers it, so the same document can be converted twice. Keep `ack_wait_secs` above the longest time an event can wait for a slot (the queue ahead of it divided by the watcher's concurrency, times a conversion); do not lower it toward the length of one conversion.
* **Token preflight.** `hs scribe watch-events` makes one authenticated `GET /info` to each configured scribe server before pulling any event. A wrong or missing `HS_BACKEND_TOKEN` (401 carrying the server's `realm="hs-scribe"`) stops the watcher there with exit 1; an unreachable server, a 403, or a 401 from a proxy/WAF is only logged (not a verdict on the credential).
* **Size `MemoryMax` per scribe unit (operational; none is generated).** A compressed object-stream bomb makes pdfium (and any PDF parser) inflate ~2x the decoded size in RAM (measured: a 1.5 MB file costs ~2.5 GB). Recommendation: `MemoryMax=` = host RAM minus what the other tenants need (e.g. the VLM backend, distill), plus `MemoryHigh=` ~10% below it, on `hs-serve-scribe*` and the watcher unit; the OOM then kills one unit, not the host.
* A scribe client whose `HS_BACKEND_TOKEN` is rejected (401/403) stops the watcher with a non-zero exit and an ERROR naming the variable instead of NAK-looping every event.
* EPUBs: one reader serves the watcher, `hs scribe inbox` and `hs personal add`, all under the configured `scribe.epub.*` limits (the inbox and personal ingest used the defaults before). `max_total_bytes` now counts bytes inflated **plus** bytes produced; a chapter the spine repeats is read once; package XML that is not well-formed or nests past 64 elements, and a chapter whose elements nest past 512, fail that book (HTML: `html_parse_error`).
* Concurrency, dpi, timeout values must be >= 1 and `timeout_policy.floor_secs <= ceiling_secs`; `scribe.servers[].concurrency >= 1`.
* The concurrency settings (`vlm_concurrency`, `page_parallel`, `parallel`) must also fit a semaphore (`Semaphore::MAX_PERMITS`); a larger value stops the server at start instead of panicking every conversion.
* **Stalled and oversized inputs.** A scribe upload that delivers no bytes for 60 s is refused with `408` and gives its slot back (a half-open connection used to hold it until a restart). The watcher reads a stored source's size first and refuses one over 256 MiB for good (`pdf_parse_error` / `epub_parse_error` / `html_parse_error`) without loading it.
* More Escalate/NAK traffic during VLM instability is expected: failed pages now fail the conversion instead of leaving holes.

### 5. Distill hosts

* New `distill_server` keys: `collections`, `hnsw.*`, `embedding.max_length`, `ollama_timeout_secs`; `distill.index_timeout_secs`. Removed: `embedding.model`, `embedding.sparse_enabled`.
* The server refuses to start when: `embedding.batch_size > 20`, `chunk_max_tokens > 1232`, `embedding.dimension != 1024`, `personal.collection_name` is not in `collections`, or `qdrant_upsert_batch`/`parallelism` or `idle_release_secs` is 0. Each error names the key.
* CUDA stays mandatory. `/health` and `/readiness` are open but now report only up/down (no Qdrant address, no error text); everything else, including `/status`, needs the token.
* Vectors written before this release were embedded from at most 512 tokens per chunk; re-index documents gradually (indexing now replaces in place and removes stale tail chunks).
* Paper abstracts: the markdown abstract extractor now ends an abstract only at a markdown heading that starts with an `Introduction`/`Background`/`Methods`/`Results`/`Discussion`/`Conclusion(s)` label, at a standalone line of one of those labels (or `Keywords`/`References`/…), not at a structured abstract's own `Background: …` / `Results show …` lines. Entries already stamped keep their old, possibly truncated text; re-extract with `hs distill abstracts build --force` (see [Index and search](docs/how-to/index-and-search.md)).

### 6. Building and upgrading

* `--release` builds need `HS_RELEASE_TAG=vX.Y.Z[-pre]`; there is no `git describe` fallback.
* `hs upgrade` refuses to install a release without `<asset>.sha256`, and fails (exit 1) on version mismatch, failed health or CPU distill. Over ssh set `XDG_RUNTIME_DIR=/run/user/$(id -u)`.
* Release binaries unwind on panic (`panic = "unwind"`).

### 7. Behavior changes to expect

* `paper_search` (MCP) always returns `{"papers": [...], "provider_failures": [...]}`; `scribe_convert` re-announces existing markdown instead of re-converting; `distill_reindex` and `personal_reindex` replace vectors in place and never delete first.
* Stems beginning with `..` are rejected everywhere (an inbox file named `....pdf` goes to `corrupted/`).
* Downloads that are not PDFs (landing pages, HTML) fail that source instead of being stored.
* `hs pipeline`, `hs migrate`, `hs distill abstracts` exit 1 on any per-item error.
* `hs openalex` (hosts that keep the local OpenAlex catalog): the steps are `load` / `load-works` → `build-indexes` → `install-fts` → `build-fts`. `build-fts` no longer runs `INSTALL fts`; it only `LOAD`s the extension and, when it is missing, fails with "fts extension not installed — run `hs openalex install-fts` (needs network once)". Run `hs openalex install-fts` once per host (and again after a DuckDB version bump) on a host that can reach DuckDB's extension repository; it needs no catalog file and is a no-op when the extension is already in the running user's `~/.duckdb/extensions/`. A host where `build-fts` already ran under that user has the extension and needs nothing.
* `hs openalex load <entity>` follows the same rule as `load-works` for every entity, the dimension tables (concepts, topics, domains, fields, subfields, sources, institutions, funders, publishers) and authors included: partitions are walked newest-first, a seen-set of integer ids gates each record, and the newest copy of an id wins; later copies are skipped silently and nothing uses `INSERT OR IGNORE`. A malformed id fails the partition (authors: counts against `--max-parse-errors`). Tables the SQL loaders filled before this release kept the *oldest* copy of an id that appears in several partitions, and their partitions are logged `ok` in `_ingest_log`, so a re-run does not touch them. To rebuild one with the new rule, stop the services that hold the catalog open, empty the table and its log rows with the DuckDB CLI (`DELETE FROM <table>; DELETE FROM _ingest_log WHERE entity = '<table>';`; `hs openalex query` is read-only), and run `hs openalex load <entity>` again.
* `hs openalex load` / `load-works` refuse a partition that is *newer* than one already logged `ok` for that entity (a snapshot re-sync added a later `updated_date`): with the newest-wins rule its records would be skipped as "duplicates" while the older stored copies stayed. The error names both partitions; to take the new snapshot, empty the entity's table(s) and delete its `_ingest_log` rows (same recipe as above; for `works` also move `seen_set.bin` aside, or `load-works` refuses a checkpoint that lists IDs the emptied table no longer has), then load again. Resuming an interrupted load is unaffected.

## Earlier notes

- **`hs status`:** a scribe tier that refuses work shows the free VRAM, the
  headroom it needs and the processes holding the card, instead of only
  `backend unavailable`.
- **Paper abstracts:** a structured abstract's own `Background:` or
  `Results …` lines no longer end the abstract extracted from markdown.
  `hs distill abstracts build` and `reconcile` skip papers that already have
  an abstract, so abstracts cut short by the earlier rule are repaired only by
  `hs distill abstracts build --force`.
- **Paper configuration:** `paper.download.max_download_bytes` (default
  256 MiB) and `paper.providers.semantic_scholar.max_retry_after_secs`
  (default 30) were added.
