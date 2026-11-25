# Configuration reference

Every home-still binary reads one file, `~/.home-still/config.yaml`, and one optional secrets file, `~/.home-still/secrets.env`. This page lists every section, key and environment variable.

## Files and loading

| Path | Purpose |
|---|---|
| `~/.home-still/config.yaml` | The configuration file. |
| `~/.home-still/secrets.env` | `KEY=VALUE` secrets exported into each process's environment. See [secrets.env](#secretsenv). |
| `/etc/home-still/config.yaml` | System-wide file. Only the paper and personal loaders read it; the user file is merged over it. |
| `~/.home-still/models/` | Where bare model file names (`pp-doclayoutv3.onnx`, `slanet-plus.onnx`) are looked up. |
| `~/.home-still/cloud-secret.key` | Default gateway signing secret (`cloud.gateway.secret_path`). |
| `~/.home-still/docs-link.key` | Signing key for document links. Created on first use with mode `0600`. |

| Command | Effect |
|---|---|
| `hs config init` | Prompts for an Unpaywall email, a CORE API key and an S3 secret key, writes `config.yaml` from the default template, writes the S3 secret to `secrets.env` (mode `0600`) when given, and creates `papers/manually_downloaded/`, `markdown/` and `catalog/` under the project directory. Asks before overwriting; `--force` or `--yes` skips the question. |
| `hs config show` | Prints the effective paper configuration as YAML, or as JSON with `--output json`. |
| `hs config path` | Prints the path of `config.yaml`. |

See [CLI reference](cli.md#hs-config) for the flags.

### Loading rules

| Situation | Behaviour |
|---|---|
| The file does not exist | Every section is absent and the documented defaults apply. |
| A section is absent, or its key has no value | The documented default applies, or the component that requires the section stops and says so. |
| The file is not valid YAML, or a section does not fit its structure | The binary stops with an error naming the file and the section. No default is substituted. |
| A key that the section does not define | Depends on the section; see the table below. |
| A directory value starting with `~/` | `~/` expands to the home directory. `home.project_dir`, `home.log_dir` and `storage.local.root` must be absolute after expansion. |

Unknown keys by section:

| Sections | Unknown key |
|---|---|
| `home`, `storage`, `events`, `cloud.gateway` | Error naming the section and the valid keys. |
| `paper`, `personal`, `logs`, `scribe`, `scribe_server`, `distill`, `distill_server`, `openalex` | Warning, logged once per process, naming the file, the dotted key (`<section>.<key>`) and the valid keys at that level. The key has no effect. |
| `docs` | Ignored without a message. |

### Environment overrides

An environment variable overrides a key of the section it names. The prefix selects the section.

| Prefix | Sections | Rule |
|---|---|---|
| `HS_SCRIBE_` | `scribe`, `scribe_server` | `HS_SCRIBE_<KEY>` sets the top-level key `<key>` (lower case) of the section being loaded. `HS_SCRIBE_CONVERT_TIMEOUT_SECS` sets `scribe.convert_timeout_secs`; `HS_SCRIBE_VLM_CONCURRENCY` sets `scribe_server.vlm_concurrency`. The two sections have disjoint keys, so one prefix serves both. Nested keys (`timeout_policy.*`, `epub.*`) have no variable. |
| `HS_DISTILL_` | `distill`, `distill_server` | `HS_DISTILL_<KEY>` sets the top-level key `<key>` (lower case) of the section being loaded. `HS_DISTILL_PORT` sets `distill_server.port`. Nested keys (`embedding.*`, `hnsw.*`) have no variable. |
| `HOME_STILL_PAPER_` | `paper` | The words after the prefix name the key path, joined by `_` whether they separate levels or words inside a key: `HOME_STILL_PAPER_DOWNLOAD_PATH` is `paper.download_path`; `HOME_STILL_PAPER_DOWNLOAD_TIMEOUT_SECS` is `paper.download.timeout_secs`. |
| `HOME_STILL_STORAGE_`, `HOME_STILL_EVENTS_` | `storage`, `events` | Same rule, read by the paper loader. |
| `HOME_STILL_PERSONAL_` | `personal` | Same rule. `HOME_STILL_PERSONAL_NAMING_MAX_INPUT_TOKENS` is `personal.naming.max_input_tokens`. |

The environment wins over the file, and the file wins over the defaults. A `HS_SCRIBE_*` or `HS_DISTILL_*` variable that sets nothing is a warning naming the valid variables. A `HOME_STILL_PAPER_*` or `HOME_STILL_PERSONAL_*` variable that names no key is an error.

`HS_SCRIBE_DIAG_DIR` and `HS_SCRIBE_COLUMN_SPLIT_ACTIVE` are not keys; see [Other environment variables](#other-environment-variables).

## `home`

An unknown key is an error.

| Key | Default | Meaning |
|---|---|---|
| `project_dir` | `~/home-still` | The project directory: papers, markdown, catalog, logs and the local Qdrant data directory live under it. Must be a non-empty string; `~/` expands. |
| `log_dir` | `<project_dir>/logs` | Where each binary spools its log files. |

Without a `storage:` section the local-filesystem default (`~/home-still`) applies. If `home.project_dir` names a different directory and `storage:` is absent, loading fails until `storage.local.root` is set.

## `paper`

Read by `hs paper`, `hs-mcp` and `hs config show`.

| Key | Default | Meaning |
|---|---|---|
| `download_path` | `<project_dir>/papers` | Where downloads are written for local storage. |
| `cache_path` | `~/.home-still/cache` | Metadata and search-result cache. |
| `download.max_concurrent` | `4` | Concurrent downloads; at least 1. |
| `download.timeout_secs` | `120` | Per-file download timeout; at least 1. |
| `download.unpaywall_email` | none | Contact address sent to Unpaywall (which requires one) and, when set, to NCBI's ID converter. Also appended to the User-Agent. Without it the Unpaywall source is skipped. |
| `download.papers_prefix` | `papers` | Storage prefix that PDFs, HTML and EPUB files are written under. Must match the prefix the pipeline reads. |
| `download.max_download_bytes` | `268435456` (256 MiB) | Hard cap on one downloaded body, enforced while streaming. |
| `resilience.cb_initial_backoff_secs` | `10` | Circuit breaker initial backoff; at least 1. |
| `resilience.cb_max_backoff_secs` | `60` | Circuit breaker maximum backoff; at least the initial backoff. |
| `resilience.cb_failure_threshold` | `3` | Consecutive failures that open the breaker; at least 1. |
| `resilience.retry_max_attempts` | `5` | Retries after the first attempt. |
| `resilience.retry_min_backoff_ms` | `100` | Minimum backoff between attempts; at least 1. |
| `resilience.retry_max_backoff_secs` | `30` | Maximum backoff; at least the minimum. |

### `paper.providers`

Each provider has `base_url`, `timeout_secs` (at least 1) and `rate_limit_interval_ms` (at least 1). A `base_url` must be an `http` or `https` URL with a host and without credentials. A provider with an API key must use `https` (loopback hosts excepted). Coverage and rate limits are described in [Paper providers](paper-providers.md).

| Provider key | `base_url` default | `timeout_secs` | `rate_limit_interval_ms` | Extra keys |
|---|---|---|---|---|
| `arxiv` | `https://export.arxiv.org/api/query` | `30` | `3000` | none |
| `openalex` | `https://api.openalex.org` | `30` | `100` | `api_key` (none) |
| `semantic_scholar` | `https://api.semanticscholar.org` | `30` | `1100` | `api_key` (none); `max_retry_after_secs` (`30`, at least 1): the longest a `Retry-After` may make a request sleep |
| `europe_pmc` | `https://www.ebi.ac.uk/europepmc` | `30` | `200` | none |
| `crossref` | `https://api.crossref.org` | `30` | `100` | `mailto` (none): a single email address |
| `core` | `https://api.core.ac.uk` | `30` | `2100` | `api_key` (none): required to use CORE |

## `storage`

Where papers, markdown and the catalog live. An unknown key is an error. Read by every binary.

| Key | Default | Meaning |
|---|---|---|
| `backend` | `local` | `local` or `s3`. |
| `local.root` | `~/home-still` | Root directory for the `local` backend. `~/` expands; the result must be absolute. |
| `s3.endpoint` | empty | S3 endpoint URL, for example `http://<storage-host>:3900`. Required for `s3`. |
| `s3.bucket` | empty | Bucket for the corpus. Required for `s3`. |
| `s3.region` | empty, treated as `garage` | S3 region. |
| `s3.access_key` | empty | Access key. Supports `${VAR}` expansion. Required for `s3`. |
| `s3.secret_key` | empty | Secret key. Supports `${VAR}` expansion. Required for `s3`. |
| `s3.allow_http` | `false` | Allow a plain `http` endpoint. |

`${VAR}` in `s3.access_key` and `s3.secret_key` is replaced by the environment variable `VAR` when the storage is built. An unset variable, or an unterminated `${`, is an error naming the variable. No other key expands variables. The `s3` backend requires a binary built with the `storage-s3` feature.

## `events`

The event bus. An unknown key is an error. There is no default: the section is required by `hs paper download`, the scribe and distill watchers, `hs scribe inbox`, `hs scribe reconvert`, `hs pipeline`, `hs migrate` and `hs-mcp`. A command that needs a bus and finds no `events:` section stops and names the key to set.

| Key | Default | Meaning |
|---|---|---|
| `backend` | none (required) | `nats` connects to a NATS server with JetStream. `noop` drops publishes and cannot be consumed from; the watchers, the inbox and the pipeline commands refuse to run on it. |
| `nats.url` | `nats://localhost:4222` | NATS server URL, or several separated by commas. Must not embed `user:password@`. |
| `nats.ack_wait_secs` | `7200` | How long the broker waits for an acknowledgement or an in-progress heartbeat before redelivering an event; at least 1. Watchers send a heartbeat every third of this while a handler runs. |
| `nats.max_deliver` | `5` | Deliveries of one event before JetStream drops it; at least 1. |
| `nats.max_age_secs` | `604800` (7 days) | How long JetStream retains unacknowledged events; at least 1. |
| `nats.max_ack_pending` | `32` | Most in-flight deliveries to a consumer; at least 1. |
| `nats.drain_timeout_secs` | `120` | How long a watcher waits for running handlers after its subscription stops; at least 1. |
| `nats.credentials_file` | none | Path of a NATS `.creds` file. |
| `nats.token_env` | none | Name of the environment variable that holds a token. |
| `nats.user` | none | User name; requires `password_env`. |
| `nats.password_env` | none | Name of the environment variable that holds the password of `user`. |
| `nats.tls_ca_file` | none | PEM bundle of the CA that signed the broker's certificate. |
| `nats.tls_client_cert` | none | PEM client certificate for mutual TLS; requires `tls_client_key`. |
| `nats.tls_client_key` | none | PEM private key of `tls_client_cert`. |
| `nats.require_tls` | `false` | Refuse a broker that does not offer TLS. |

Set only one of `credentials_file`, `token_env` and `user` with `password_env`. Subjects and durable consumers are listed in [Services and ports](services-and-ports.md#event-subjects).

## `logs`

Every binary spools JSON-lines log records locally and ships closed files to storage. With `storage.backend: s3` the files go to the `logs.bucket` bucket on the same endpoint; with `local` they go to `<log_dir>/archive/`.

| Key | Default | Meaning |
|---|---|---|
| `bucket` | `logs` | Bucket name for the `s3` backend. Ignored for `local`. |
| `rotate_max_bytes` | `4194304` (4 MiB) | Close the current file when it reaches this size; at least 1. |
| `rotate_interval_secs` | `60` | Close the current file when it reaches this age; at least 1. |
| `ship_interval_secs` | `30` | How often closed files are uploaded; at least 1. |
| `spool_max_bytes` | `268435456` (256 MiB) | Most bytes of closed files kept while waiting to ship; the oldest are dropped first; at least 1. |
| `spool_max_age_secs` | `604800` (7 days) | Closed files older than this are dropped; at least 1. |

Object keys are `<service>/<host>/<YYYY>/<MM>/<DD>/<unix-ms>-<uuid>.jsonl`.

## `scribe`

The scribe client: `hs scribe`, the scribe watcher, the inbox and `hs-mcp`. Environment overrides: `HS_SCRIBE_<KEY>` for the scalar keys.

| Key | Default | Meaning |
|---|---|---|
| `servers` | empty list | The scribe servers, in dispatch order. See [Server entries](#server-entries). Commands that talk to a scribe server stop when it is empty. |
| `output_dir` | `<project_dir>/markdown` | Local markdown directory used by `hs scribe convert` for its default output path and by local migration and backfill commands. |
| `watch_dir` | `<project_dir>/papers` | Local papers directory used by `hs scribe catalog-backfill`. |
| `corrupted_dir` | `<project_dir>/corrupted` | Accepted but not read by any command. The inbox moves rejected files to the `corrupted/` storage prefix. |
| `catalog_dir` | `<project_dir>/catalog` | Local catalog directory used by local migration and backfill commands. |
| `local_server` | `true` | When `false`, skip local scribe server start-up: a machine that only forwards to remote scribe servers. |
| `inbox_poll_interval_secs` | `30` | How often `hs scribe inbox run` polls `papers/manually_downloaded/` on the configured storage; at least 1. |
| `convert_timeout_secs` | `900` | Baseline request timeout for a conversion when the page count is unknown; at least 1. |
| `timeout_policy.base_secs` | `60` | Fixed overhead of a conversion deadline. |
| `timeout_policy.per_page_secs` | `15` | Deadline budget per PDF page. |
| `timeout_policy.floor_secs` | `300` | Minimum deadline; at least 1 and at most `ceiling_secs`. |
| `timeout_policy.ceiling_secs` | `3600` | Maximum deadline. |
| `epub.max_entries` | `10000` | Most entries an EPUB archive may hold; at least 1. |
| `epub.max_entry_bytes` | `67108864` (64 MiB) | Largest any one entry may expand to; at least 1 and at most `epub.max_total_bytes`. |
| `epub.max_total_bytes` | `268435456` (256 MiB) | Most bytes one conversion may inflate and produce; at least 1. |
| `epub.html.max_input_bytes` | `16777216` (16 MiB) | Largest markup accepted by any HTML parse. |
| `epub.html.max_nesting` | `512` | Deepest element nesting. |
| `epub.html.max_nodes` | `1000000` | Most nodes in the parsed tree; each attribute counts as one. |
| `epub.html.max_attributes_per_element` | `1024` | Most attributes on one element. |
| `epub.html.max_attributes` | `1000000` | Most attributes in a document. |
| `epub.html.max_convert_secs` | `60` | Wall-clock budget of one HTML or EPUB conversion. |

A conversion's deadline is `clamp(base_secs + pages × per_page_secs, floor_secs, ceiling_secs)`. The client sends it as the request timeout and in the `X-Convert-Deadline-Secs` header. A PDF whose page count cannot be read is refused before dispatch. An archive over an `epub` cap is refused, not truncated.

### Server entries

An entry in `servers` is either a bare URL or a mapping.

| Form | Meaning |
|---|---|
| `http://<scribe-host>:7433` | Backend `glm_ocr`, concurrency `4`. |
| `{url, backend, concurrency}` | Explicit values. |

| Field | Default | Meaning |
|---|---|---|
| `url` | required | Base URL of the scribe server. |
| `backend` | `glm_ocr` | Backend name recorded in the catalog as the converter. Entries with the same name form one tier. |
| `concurrency` | `4` | Conversions dispatched to this entry at once; at least 1. A tier's cap is the sum over its entries. |

```yaml
scribe:
  servers:
    - url: http://<gpu-host>:7435
      backend: olmocr
      concurrency: 2
    - url: http://<scribe-host>:7433
      backend: glm_ocr
      concurrency: 8
```

Tiers are tried in order of first appearance; within a tier a document goes to the least-loaded host. See [Architecture](../explanation/architecture.md).

## `scribe_server`

The scribe server (`hs serve scribe`, `hs-scribe-server`). Every key is also an environment variable: `HS_SCRIBE_<KEY>` with the key in upper case. The environment wins over the file. The server stops at start on an invalid value and names the key.

### Backend and connection

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `backend` | `HS_SCRIBE_BACKEND` | `Ollama` | VLM backend: `Ollama`, `OpenAi` or `Cloud`. |
| `model` | `HS_SCRIBE_MODEL` | `glm-ocr:latest` | Model name for the Ollama and OpenAI-compatible backends. |
| `ollama_url` | `HS_SCRIBE_OLLAMA_URL` | `http://localhost:11434` | Ollama URL. |
| `openai_url` | `HS_SCRIBE_OPENAI_URL` | `http://localhost:8080` | OpenAI-compatible server URL. |
| `openai_api_key` | `HS_SCRIBE_OPENAI_API_KEY` | none | Bearer token for the OpenAI-compatible backend, for a proxy that requires one. |
| `cloud_url` | `HS_SCRIBE_CLOUD_URL` | `https://api.z.ai/api/paas/v4/layout_parsing` | Cloud API endpoint. |
| `cloud_api_key` | `HS_SCRIBE_CLOUD_API_KEY` | none | Bearer token for the `Cloud` backend. |
| `vlm_idle_timeout_secs` | `HS_SCRIBE_VLM_IDLE_TIMEOUT_SECS` | `300` | Longest silence tolerated on a VLM connection: the wait for the first byte and the gap between reads of a streaming answer; at least 1. |
| `ollama_request_timeout_secs` | `HS_SCRIBE_OLLAMA_REQUEST_TIMEOUT_SECS` | `600` | Longest one Ollama generate call may take; at least 1, and with the Ollama backend at most `max_convert_deadline_secs`. |

### Deadlines and limits

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `convert_deadline_secs` | `HS_SCRIBE_CONVERT_DEADLINE_SECS` | `900` | Wall-clock deadline of one conversion on the server; at least 1. Cancels every in-flight VLM request when it fires. |
| `max_convert_deadline_secs` | `HS_SCRIBE_MAX_CONVERT_DEADLINE_SECS` | `7200` | Ceiling on the per-request `X-Convert-Deadline-Secs` a caller may ask for. A larger request is clamped. Must be at least `convert_deadline_secs`. |
| `max_render_pixels` | `HS_SCRIBE_MAX_RENDER_PIXELS` | `36000000` | Ceiling on the pixels of one rendered page. A page whose media box times the dpi exceeds it is rendered at a lower dpi; an implausible page box is refused. At least 1. |
| `vram_headroom_mb` | `HS_SCRIBE_VRAM_HEADROOM_MB` | `15000` | Free VRAM required to cold-start the model. Below it, and with the model not resident, `/health` and `/readiness` refuse work. Hosts without an NVIDIA GPU skip the check. |
| `gpu_yield_file` | none | unset | A host GPU broker's yield file (`{"for","since","expires"}`). While it names an unexpired tenant, `/health` and `/readiness` refuse work even with the model resident, so in-flight conversions drain and the broker can evict the model. `/health` reports the tenant as `backend_yielding_to`. |
| `gpu_make_room_cmd` | none | unset | Argv run in the background, with `--need-mib <vram_headroom_mb>` appended and at most every 30 s, whenever the gate refuses work. The broker decides whether to free the card. |

### Pipeline and performance

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `pipeline_mode` | `HS_SCRIBE_PIPELINE_MODE` | `PerRegion` | `PerRegion` (layout detection, then per-region recognition) or `FullPage` (whole-page recognition). |
| `dpi` | `HS_SCRIBE_DPI` | `200` | PDF rendering resolution; at least 1. |
| `max_image_dim` | `HS_SCRIBE_MAX_IMAGE_DIM` | `1800` | Downscale images larger than this many pixels; at least 1. |
| `vlm_concurrency` | `HS_SCRIBE_VLM_CONCURRENCY` | host class, see [Host classes](#host-classes) | With the `legacy` converter: the shared VLM-call semaphore, the number of conversions admitted at once (further uploads get 503) and the slot total `/readiness` advertises. With `olmocr`: concurrent `olmocr` runs. At least 1. |
| `region_parallel` | `HS_SCRIBE_REGION_PARALLEL` | host class | Concurrent regions within one page; at least 1. |
| `page_parallel` | `HS_SCRIBE_PAGE_PARALLEL` | `2` | Concurrent pages in flight within one conversion; at least 1. The worst-case concurrent VLM calls are the dispatcher concurrency times `page_parallel` times `region_parallel`. |
| `parallel` | `HS_SCRIBE_PARALLEL` | `1` | Concurrent pages in `FullPage` mode; at least 1. |
| `use_cuda` | `HS_SCRIBE_USE_CUDA` | `true`; `false` on macOS | Run ONNX layout detection on CUDA. `true` on macOS is a start-up error. |
| `layout_model_path` | `HS_SCRIBE_LAYOUT_MODEL_PATH` | `pp-doclayoutv3.onnx` | Layout model. A bare file name is looked up in `~/.home-still/models/`; an existing path is used as given. |
| `table_model_path` | `HS_SCRIBE_TABLE_MODEL_PATH` | `slanet-plus.onnx` | Table-structure model, resolved the same way. |

`vlm_concurrency`, `page_parallel` and `parallel` must not exceed the number of permits a Tokio semaphore can hold.

### olmocr converter

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `converter` | `HS_SCRIBE_CONVERTER` | `legacy` | `legacy` (the per-region pipeline) or `olmocr` (shell out to the `olmocr` CLI). |
| `olmocr_bin` | `HS_SCRIBE_OLMOCR_BIN` | `olmocr` | Path of the `olmocr` CLI; a bare name is looked up on `PATH`. The server refuses to start when it is not an executable file. |
| `olmocr_endpoint` | `HS_SCRIBE_OLMOCR_ENDPOINT` | `http://localhost:8081/v1` | OpenAI-compatible endpoint the CLI talks to. |
| `olmocr_model` | `HS_SCRIBE_OLMOCR_MODEL` | `olmocr` | Model name the endpoint serves. |

With `converter: olmocr`, `olmocr_bin`, `olmocr_endpoint` and `olmocr_model` must be non-empty. The converter returns assembled markdown; the per-region pipeline, the repetition detector and the quality post-processing do not run. See [Conversion](../explanation/conversion.md).

### Host classes

The server detects a host class from the CPU count, memory and GPU and uses it for the defaults that the file and the environment do not set. `HS_HOST_CLASS` pins the class to one of `pi`, `apple_low`, `apple_high`, `nvidia_mid`, `nvidia_high` or `generic_cpu`; any other value is ignored with a warning.

| Class | Hardware | `vlm_concurrency` | `region_parallel` | Distill `concurrency` |
|---|---|---|---|---|
| `pi` | at most 4 cores, no GPU, at most 8 GB RAM | `2` | `2` | `2` |
| `apple_low` | Apple Silicon, at most 16 GB | `4` | `3` | `4` |
| `apple_high` | Apple Silicon, more than 16 GB | `6` | `4` | `6` |
| `nvidia_mid` | NVIDIA, under 24000 MB VRAM | `8` | `3` | `8` |
| `nvidia_high` | NVIDIA, 24000 MB VRAM or more | `12` | `3` | `8` |
| `generic_cpu` | no GPU, more than 4 cores or more than 8 GB RAM | `4` | `2` | CPU count divided by 4, at least 2 |

## `distill`

The distill client: `hs distill`, the distill watcher, `hs pipeline` and `hs-mcp`. Environment overrides: `HS_DISTILL_<KEY>` for the top-level keys.

| Key | Default | Meaning |
|---|---|---|
| `servers` | empty list | Distill server URLs, for example `http://<gpu-host>:7434`. Commands that talk to a distill server stop when it is empty. |
| `markdown_dir` | `<project_dir>/markdown` | Local markdown directory for local indexing. |
| `catalog_dir` | `<project_dir>/catalog` | Local catalog directory for local indexing. |
| `concurrency` | host class | Documents the distill watcher indexes in parallel; at least 1. See [Host classes](#host-classes). |
| `index_timeout_secs` | `1800` | Deadline for one indexing request; at least 1. |

## `distill_server`

The distill server (`hs serve distill`, `hs-distill-server`). The server refuses to start on an invalid value and names the key.

| Key | Default | Meaning |
|---|---|---|
| `host` | `0.0.0.0` | Bind address. The `--host` flag wins. |
| `port` | `7434` | Listen port. The `--port` flag wins; `hs serve distill --port` passes it as `HS_DISTILL_PORT`. Must not be 0. |
| `qdrant_url` | `http://localhost:6334` | Qdrant gRPC endpoint. |
| `qdrant_data_dir` | `<project_dir>/data/qdrant` | Qdrant storage directory. |
| `collection_name` | `academic_papers` | Default collection; a request that names none uses it. |
| `collections` | `[paper_abstracts, personal_docs]` | Further collections a request may name. Every configured collection is created or verified at start-up; a request naming any other collection gets HTTP 400. |
| `chunk_max_tokens` | `1000` | Maximum tokens per chunk. |
| `chunk_overlap` | `100` | Token overlap between chunks; smaller than `chunk_max_tokens`. |
| `qdrant_upsert_batch` | `1000` | Chunks per Qdrant upsert request; at least 1. |
| `qdrant_upsert_parallelism` | `4` | Upsert requests in flight per document; at least 1. |
| `llm_metadata` | `false` | Extract keywords and topics with Ollama. A failed call fails that document. |
| `metadata_model` | `llama3.2:latest` | Ollama model for metadata. Must be non-empty when `llm_metadata` is `true`. |
| `ollama_url` | `http://localhost:11434` | Ollama URL including the port. Used only when `llm_metadata` is `true`. |
| `ollama_timeout_secs` | `120` | Upper bound on one metadata call; at least 1 when `llm_metadata` is `true`. |
| `embedding.dimension` | `1024` | Expected vector width. Compared with what the model returns at start-up; a mismatch stops the server. |
| `embedding.max_length` | `1280` | Tokens the model sees per text; longer input is truncated. `chunk_max_tokens + 48` must fit. At most `8192`. |
| `embedding.batch_size` | unset (starts at 32) | Rows per forward pass, and the starting point when `adaptive_batch` is on. At least 1 and at most 128 × 512² ÷ `max_length²` rows. |
| `embedding.pool_size` | unset (1) | Model copies; at least 1. |
| `embedding.adaptive_batch` | `true` | Tune the batch size in-process against observed throughput. A batch the accelerator cannot allocate also tunes it, downward. |
| `embedding.compute_device` | `cuda` | Only `cuda` is accepted; any other value fails to load. |
| `embedding.idle_release_secs` | unset | Drop the model from GPU memory after this many idle seconds; at least 1. The next request reloads it. Unset keeps it resident. |
| `embedding.vram_floor_mb` | `5000` | Free VRAM required before (re)loading the model. Below it the load fails and names the GPU holders. Hosts without an NVIDIA GPU skip the check. |
| `embedding.gpu_yield_file` | unset | A host GPU broker's yield file (`{"for","since","expires"}`). While it names an unexpired tenant a (re)load fails with `gpu busy: yielded to <tenant>`, so the event is NAK'd and redelivered. |
| `embedding.gpu_make_room_cmd` | unset | Argv run once, with `--need-mib <vram_floor_mb>` appended, when a (re)load is blocked; exit 0 means "try again now". |
| `hnsw.m` | `16` | Edges per node for new collections; 4 to 128. |
| `hnsw.ef_construct` | `100` | Candidates considered while building the graph; from `m` to 1024. |
| `hnsw.search_ef` | `128` | Candidates considered per query; 1 to 4096. |
| `hnsw.max_indexing_threads` | `4` | Threads Qdrant may use when HNSW is enabled on an existing collection; 1 to 64. |

Keys `embedding.model` and `embedding.sparse_enabled` have no effect; the server logs a warning when they are set. The HNSW settings apply when a collection is created; an existing collection keeps the graph it was built with.

Collection names are 1 to 64 characters of `[A-Za-z0-9_-]`, starting with a letter or digit, and must be unique across `collection_name` and `collections`.

### `HS_DISTILL_<KEY>` overrides

The variable name is `HS_DISTILL_` followed by the key in upper case. The loader lower-cases the name and sets that top-level key of the section being loaded: `distill_server` when the server starts, `distill` for the client. The environment provider parses the value: `true` and `false`, numbers and quoted strings take their natural types.

| Variable | Effect |
|---|---|
| `HS_DISTILL_PORT` | `distill_server.port`. |
| `HS_DISTILL_QDRANT_URL` | `distill_server.qdrant_url`. |
| `HS_DISTILL_CONCURRENCY` | `distill.concurrency`. |

A name that is a top-level key of neither section is a warning. Nested keys such as `embedding.max_length` can only be set in the file.

## `docs`

Document links. See [Open documents with links](../how-to/open-documents-with-links.md). Read by `hs docs link`, `hs serve docs` and `hs-mcp`. `hs docs link` needs `lan_base_url` (or `--base-url`); the MCP tools attach links only when both base URLs are set and the signing key can be loaded. Every host that mints links must hold the same `~/.home-still/docs-link.key` as the docs service, which verifies only its own key.

| Key | Default | Meaning |
|---|---|---|
| `lan_base_url` | none | Base URL that LAN links point at, for example `http://<docs-host>:7446`. |
| `remote_base_url` | none | Base URL that remote links point at, for example `https://<gateway-host>`. |
| `link_ttl_secs` | `3600` | How long a link works. |
| `default_subject` | this machine's host name | The subject links are bound to. |

`hs serve docs` requires a `storage:` section.

## `cloud.gateway`

The gateway (`hs-gateway`). There is no default gateway: a missing `cloud.gateway` section stops it. An unknown key is an error, except `key_rotation_days`, which is ignored with a warning.

| Key | Default | Meaning |
|---|---|---|
| `listen` | required | Address to listen on, for example `0.0.0.0:7440`. |
| `secret_path` | `~/.home-still/cloud-secret.key` | HMAC signing secret. The admin key (`cloud-admin.key`) and the revocation list (`cloud-revoked.json`) live beside it. |
| `previous_secret_path` | none | Previous signing secret, set during a rotation grace period: tokens signed with it are still accepted, and new tokens use the current secret. Must exist when set. |
| `token_ttl_secs` | `14400` (4 hours) | Access-token lifetime; at least 1. |
| `refresh_ttl_secs` | `604800` (7 days) | Refresh-token lifetime; at least 1. |
| `routes` | required | Map from service name to one backend URL or a list of URLs. Requests are balanced across the instances of a service. |
| `max_concurrent_proxy_requests` | `64` | Proxied requests in flight at once; further requests get 503. At least 1. |
| `max_request_body_bytes` | `268435456` (256 MiB) | Largest request body the proxy streams to a backend; at least 1. |
| `backend_connect_timeout_secs` | `10` | Backend connect timeout; at least 1. |
| `backend_read_timeout_secs` | `600` | Longest wait for response headers or for the next chunk of a response body; at least 1. |
| `backend_total_timeout_secs` | `3600` | Longest one proxied request may take end to end; at least 1. |
| `backend_failure_cooldown_secs` | `10` | How long an instance that refused a connection is skipped. |
| `auth_rate_limit_per_minute` | `30` | POSTs per minute allowed on each of `/cloud/enroll`, `/authorize`, `/token` and `/register`; at least 1. |

`routes` must contain a backend for each of `scribe`, `distill`, `mcp` and `docs`; a missing service stops the gateway and names the key to add. A route is `http://host[:port]` or `https://host[:port]` with no credentials, path, query or fragment, and an instance may be listed once per service.

```yaml
cloud:
  gateway:
    listen: 0.0.0.0:7440
    routes:
      scribe: [http://<gpu-host>:7433, http://<scribe-host>:7433]
      distill: http://<gpu-host>:7434
      mcp: http://127.0.0.1:7445
      docs: http://<docs-host>:7446
```

## `openalex`

The local OpenAlex database. See [Use the OpenAlex catalog](../how-to/use-the-openalex-catalog.md).

| Key | Default | Meaning |
|---|---|---|
| `db_path` | required | Path of the DuckDB file. `~/` expands. Must be non-empty. |
| `snapshot_dir` | required by `hs openalex` | Directory that holds the OpenAlex snapshot. `hs-mcp` does not read it. |

`hs openalex` stops when the section, `db_path` or `snapshot_dir` is missing. `hs-mcp` offers the `openalex_*` tools only when the database file exists and its build is complete; see [MCP tools](mcp-tools.md#tool-visibility).

## `personal`

The personal-documents store. See [Manage personal documents](../how-to/manage-personal-documents.md). Environment overrides use `HOME_STILL_PERSONAL_`. The store's files live in the backend that `storage:` selects.

| Key | Default | Meaning |
|---|---|---|
| `collection_name` | `personal_docs` | Qdrant collection for the store. Must be non-empty. |
| `storage_dir` | `personal` | Directory under the project directory that holds the store. |
| `ingest_inbox` | `inbox` | Directory that `personal_add` resolves file names against. A relative value is joined under `<project_dir>/<storage_dir>`; an absolute value is used as given. |
| `distill_url` | `http://localhost:7434` | Distill server URL. |
| `scribe_url` | `http://localhost:7433` | Scribe server URL. |
| `categories` | all nine | Categories the store accepts, a subset of `medical`, `financial`, `education`, `legal`, `employment`, `tax`, `insurance`, `correspondence` and `other`. Must list at least one. An ingest outside the list fails. |
| `naming.ollama_url` | `http://localhost:11434` | Ollama URL used to name and categorise documents. |
| `naming.model` | `qwen2.5:7b` | Ollama model used for naming. |
| `naming.max_input_tokens` | `2048` | Tokens of a document sent to the naming model. |

## secrets.env

`secrets.env` holds `KEY=VALUE` lines. Each binary reads it at start-up, before any other thread exists, and exports every entry that is not already set in the environment. A variable that is already set wins over the file. A missing file is not an error; a NUL byte in an entry is.

| Syntax | Meaning |
|---|---|
| `KEY=VALUE` | An entry. An optional leading `export ` is accepted. |
| `# text` | A comment line. |
| `KEY="value" # note` or `KEY='value'` | A quoted value; a comment may follow the closing quote. |
| `KEY=value # note` | A trailing ` #` comment is stripped from an unquoted value. |

The file should be mode `0600`; a more permissive mode prints a warning naming the file.

| Variable | Read by | Meaning |
|---|---|---|
| `HS_BACKEND_TOKEN` | the gateway, every backend service, every client | Shared secret of at least 32 bytes of visible ASCII. Every backend other than a health probe requires `Authorization: Bearer <token>`, and every client sends it. Set the same value on the gateway, on every backend and on every client host. |
| `HS_S3_ACCESS_KEY`, `HS_S3_SECRET_KEY` | storage | Referenced as `${HS_S3_ACCESS_KEY}` and `${HS_S3_SECRET_KEY}` in `storage.s3`. `hs config init` writes `HS_S3_SECRET_KEY`. |
| the variable that `events.nats.token_env` or `events.nats.password_env` names | the event bus | NATS token or password. |

Variable names for `${…}` expansion and for the NATS `*_env` keys are chosen by the configuration; the names above are the conventional ones.

## Other environment variables

| Variable | Read by | Meaning |
|---|---|---|
| `HS_HOST_CLASS` | scribe and distill defaults | Pin the [host class](#host-classes). |
| `HS_MCP_URL` | `hs` | MCP endpoint that `hs` commands call directly, without the gateway and without authentication, for example `http://localhost:7445/mcp`. When unset, `hs` uses the enrolled gateway. |
| `HS_SCRIBE_DIAG_DIR` | scribe client | When set, each successful PDF conversion writes `<dir>/<stem>.diag.jsonl` with one record per page and a summary line. |
| `HS_SCRIBE_COLUMN_SPLIT_ACTIVE` | scribe server | `1`, `true` or `yes` (also `TRUE`, `YES`) turns on column splitting. Read at each conversion. Off by default. |
| `HS_RELEASE_TAG` | release builds | Version string baked into a release build. See [CONTRIBUTING](../../CONTRIBUTING.md). |
