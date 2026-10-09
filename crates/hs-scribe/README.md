# hs-scribe

Turn academic PDFs into clean markdown. Layout-aware, table-aware, GPU-accelerated.

Part of [home-still](../../README.md) -- free tools for knowledge acquisition and distillation.

## How it works

hs-scribe uses a multi-stage pipeline to understand the structure of each page before extracting text:

1. **PDF rendering** -- Pages are rendered to images at configurable DPI (default 200) using PDFium.
2. **Layout detection** -- PP-DocLayout-V3 via ONNX Runtime identifies 25 region types on each page: titles, paragraphs, figures, tables, formulas, references, footnotes, and more. Regions are returned with native reading order.
3. **Table structure** -- For table regions, SLANet-Plus detects cell boundaries so each cell can be OCR'd individually and reassembled as HTML.
4. **VLM OCR** -- Each region is sent to a vision-language model (GLM-OCR by default) with a task-specific prompt. Text regions get `"OCR:"`, tables get `"Table Recognition:"`, formulas get `"Formula Recognition:"`.
5. **Markdown assembly** -- Regions are sorted by reading order and assembled into section-aware markdown with heading hierarchy, HTML tables, and formula blocks.

The pipeline has two modes:
- **PerRegion** (default): Full layout detection + per-region OCR. Best quality for complex academic papers.
- **FullPage**: Skip layout detection, send the entire page to the VLM. Faster, uses less memory, but no table structure or region-specific prompts.

## Quick start

```sh
# Run a scribe server on a GPU host (models, libpdfium and HS_BACKEND_TOKEN in place; see below)
hs serve scribe

# Convert a PDF
hs scribe convert paper.pdf --out paper.md

# Convert PDFs automatically as `papers.ingested` events arrive on the event bus
hs scribe watch-events
```

## Setup

Nothing provisions a scribe host for you: `hs serve scribe` starts the server and provisions nothing. The scribe host needs, before it starts:

1. **The layout model** (PP-DocLayout-V3, ~125 MB ONNX file) at the configured model path (Legacy converter), and a VLM backend reachable at the configured URL (Ollama, an OpenAI-compatible server such as llama-swap, or a cloud API).
2. **libpdfium** and **`HS_BACKEND_TOKEN`** (below).
3. **GPU acceleration** chosen by the server's configuration: Metal via a native Ollama on Apple Silicon, CUDA on NVIDIA hosts (`HS_SCRIBE_USE_CUDA`).

`hs serve scribe --install` installs the server as a systemd (Linux) or launchd (macOS) unit that reads `~/.home-still/secrets.env`.

### Requirements of a scribe host

- **`HS_BACKEND_TOKEN`.** `hs-scribe-server` refuses to start unless `HS_BACKEND_TOKEN` (≥ 32 visible ASCII bytes, e.g. `openssl rand -hex 32`; the same value as the gateway and every client host) is set — put it in `~/.home-still/secrets.env`, on every scribe host (the macOS launchd one included) and on every host that runs a scribe client (`hs`, `hs-mcp`). Every route except `GET /health` and `GET /readiness` requires `Authorization: Bearer <token>`; anything else, including a path no route serves, gets a 401 with a JSON body. `ScribeClient` sends the token automatically from the same variable. There is no unauthenticated mode: this is the endpoint that parses untrusted PDFs and drives the GPU.
- **libpdfium.** Every PDF is counted by pdfium before it is dispatched or converted, on every converter (olmocr included), and the watcher (`hs scribe watch-events`) counts the PDFs it dispatches the same way. `hs-scribe-server` and the watcher refuse to start when libpdfium cannot be bound. It is looked up in `~/.local/lib` and `~/.home-still/dyld-libs`, then the system library path (never the working directory). Container images bundle it. `hs` and `hs-mcp` hosts need it too for `hs scribe convert` and the `scribe_convert` tool on PDFs (those fail with the same error when it is missing).

### Platform behavior

| Platform | VLM runs on | GPU acceleration |
|---|---|---|
| macOS Apple Silicon | Native Ollama on the host | Metal GPU |
| Linux + NVIDIA | Ollama (or an OpenAI-compatible server) you run | CUDA |
| Linux / macOS Intel | Ollama you run | CPU only |

On Apple Silicon, run the server natively (`hs serve scribe`) next to a native Ollama so the VLM uses Metal. The optional Docker image (below) reaches a host Ollama via `host.docker.internal:11434`; Docker on macOS cannot use the GPU.

## Commands

### Convert a PDF

```sh
hs scribe convert paper.pdf              # markdown to stdout
hs scribe convert paper.pdf --out paper.md  # markdown to file
hs scribe convert paper.pdf --server http://remote:7433  # use a remote server
```

During conversion, you'll see a live progress bar with elapsed time and ETA:

```
Converting ━━━━━━━━━━━━━━╸              22/43  00:01:30 ETA 00:00:42  [vlm] OCR region 5/12 on page 22
```

### Watch for new PDFs

```sh
hs scribe watch-events               # foreground; needs events.backend: nats
hs serve scribe-watch --install      # install as a user-level service
```

Subscribes to `papers.ingested`, converts each PDF through the configured scribe servers and uploads the markdown back to storage.

### Manage the server

```sh
hs serve scribe start    # run in the background
hs serve scribe stop     # stop it
hs status                # health of every configured scribe server
curl http://<host>:7433/health   # liveness of one server
```

## Architecture

```
hs scribe convert paper.pdf
    |
    v
ScribeClient (HTTP multipart upload to localhost:7433)
    |
    v
hs-scribe-server (native process or Docker image)
    |--- PDF rendering (PDFium, configurable DPI)
    |--- Layout detection (ONNX: PP-DocLayout-V3, 25 region types)
    |--- Table structure (ONNX: SLANet-Plus, cell boundaries)
    |--- VLM OCR (Ollama / OpenAI-compat / Cloud)
    |         |
    |         +-- Metal GPU on macOS (native Ollama)
    |         +-- CUDA on Linux (your Ollama / llama-swap)
    |
    v
NDJSON progress stream --> final markdown
```

The server streams progress as newline-delimited JSON so the CLI can show real-time updates:
- `[parse]` Parsing PDF pages
- `[layout]` Detecting layout on each page (region count, table count)
- `[vlm]` OCR per region and per table cell with counts
- `[done]` Assembling final markdown

## VLM backends

| Backend | Environment variable | Use case |
|---|---|---|
| **Ollama** (default) | `HS_SCRIBE_BACKEND=Ollama` | Local inference. Uses Metal on macOS, CPU/CUDA on Linux |
| **OpenAI-compatible** | `HS_SCRIBE_BACKEND=OpenAi` | vLLM, sglang, MLX-LM, or any `/v1/chat/completions` server |
| **Cloud** | `HS_SCRIBE_BACKEND=Cloud` | Remote API with bearer token auth |

## Configuration

### Client config (`~/.home-still/config.yaml`)

The `scribe` section configures the CLI client — where to save output, where to watch, and which servers to use:

```yaml
scribe:
  output_dir: ~/markdown
  watch_dir: ~/papers
  servers:
    - http://localhost:7433
    - http://gpu-server:7433
    - http://pi-cluster:7433
```

With multiple servers, `hs scribe convert` and `hs scribe watch-events` automatically load-balance across them. The CLI queries each server's `/readiness` endpoint and routes each PDF to the server with the most available VLM slots.

The configured `scribe.servers` list is the only source of server addresses; there is no service registry.

EPUBs (the watcher, `hs scribe inbox` and `hs personal add` all read them through one bounded reader) are limited by:

```yaml
scribe:
  epub:
    max_entries: 10000          # entries in the archive
    max_entry_bytes: 67108864   # any one entry, after decompression (64 MiB)
    max_total_bytes: 268435456  # bytes inflated plus bytes produced by one conversion (256 MiB)
```

Every HTML parse (HTML sources and EPUB chapters) is bounded by `scribe.epub.html.{max_input_bytes (16 MiB), max_nesting (512), max_nodes (1000000, attributes count as one node each), max_attributes_per_element (1024), max_attributes (1000000), max_convert_secs (60)}`; nesting is also measured on the finished tree, because html5ever moves subtrees after attaching them: html5ever is driven in 4 KiB steps through a sink that measures the real tree, and parsing stops inside the parse when a bound is crossed or the conversion's wall-clock budget expires.

Each chapter is read once however often the spine lists it, only the entries the reader needs are decompressed, and package XML nested more than 64 elements deep, or a chapter nested more than 512 elements deep, is refused. Exceeding a limit fails that book; nothing is truncated.

### Server config (environment variables)

Server-side settings use environment variables with the `HS_SCRIBE_` prefix (`HS_SCRIBE_VLM_CONCURRENCY` overrides `vlm_concurrency`). The same keys can be set, one level down, in the `scribe_server:` section of `~/.home-still/config.yaml` (the one config file every home-still binary reads); the environment wins over the file. The client side (`hs scribe …`) reads the `scribe:` section and the same `HS_SCRIBE_<KEY>` variables (`HS_SCRIBE_CONVERT_TIMEOUT_SECS` → `scribe.convert_timeout_secs`). A malformed section or an invalid value stops the server at start with the key named; nothing falls back to defaults.

### Core settings

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_BACKEND` | `Ollama` | VLM backend: `Ollama`, `OpenAi`, or `Cloud` |
| `HS_SCRIBE_MODEL` | `glm-ocr:latest` | Model name for Ollama/OpenAI backends |
| `HS_SCRIBE_PIPELINE_MODE` | `PerRegion` | `PerRegion` (layout + per-region OCR) or `FullPage` (whole-page OCR) |
| `HS_SCRIBE_DPI` | `200` | PDF rendering resolution. Lower = faster but less detail |

### Connection settings

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_OLLAMA_URL` | `http://localhost:11434` | Ollama server URL |
| `HS_SCRIBE_OPENAI_URL` | `http://localhost:8080` | OpenAI-compatible server URL |
| `HS_SCRIBE_CLOUD_URL` | `https://api.z.ai/...` | Cloud API endpoint |
| `HS_SCRIBE_CLOUD_API_KEY` | *(none)* | Bearer token for cloud backend |
| `HS_SCRIBE_VLM_IDLE_TIMEOUT_SECS` | `300` | Longest silence tolerated on a VLM backend connection (first byte, then between reads of a streaming answer) |
| `HS_SCRIBE_OLLAMA_REQUEST_TIMEOUT_SECS` | `600` | Longest one Ollama generate call may take (Ollama backend only) |
| `HS_SCRIBE_CONVERT_DEADLINE_SECS` | `900` | Wall-clock deadline of one conversion on the server |

### Performance tuning

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_VLM_CONCURRENCY` | host-class default (e.g. `4` on low-end Apple Silicon, `2` on a Pi) | Legacy: the shared VLM-call semaphore across all conversions, the number of conversions admitted at once (more get 503) and the `vlm_slots_total` `/readiness` advertises. olmocr: concurrent `olmocr` runs. In Legacy, PDF open/render/layout (stage 1) is serialized by pdfium-render's process-wide lock, so only the VLM stage overlaps between conversions; values > 1 are intended (they pipeline VLM work with the next render). A wedged pdfium call makes the watchdog exit the whole server, ending all in-flight conversions (see `docs/deployment.md`) |
| `HS_SCRIBE_REGION_PARALLEL` | host-class default (`2` Pi, `3` low-end Apple Silicon and NVIDIA, `4` high-end Apple Silicon) | Max concurrent regions within one page |
| `HS_SCRIBE_PARALLEL` | `1` | Max concurrent pages in FullPage mode |
| `HS_SCRIBE_USE_CUDA` | `true` (`false` on macOS) | Enable CUDA for ONNX layout detection. `true` on macOS is a startup error: CUDA is not available there |
| `HS_SCRIBE_MAX_IMAGE_DIM` | `1800` | Downscale images larger than this (pixels) |

### Model paths

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_LAYOUT_MODEL_PATH` | `pp-doclayoutv3.onnx` | PP-DocLayout-V3 ONNX model |
| `HS_SCRIBE_TABLE_MODEL_PATH` | `slanet-plus.onnx` | SLANet-Plus ONNX model |

A bare model filename is looked up in `~/.home-still/models/` (an existing path is used as given).

### olmocr converter

`HS_SCRIBE_CONVERTER=olmocr` makes the server shell out to the `olmocr` CLI instead of running the per-region pipeline. The server refuses to start when `olmocr_bin` is not an executable file (or not on `PATH`).

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_CONVERTER` | `legacy` | `legacy` (per-region pipeline) or `olmocr` |
| `HS_SCRIBE_OLMOCR_BIN` | `olmocr` | Path of the `olmocr` CLI (a bare name is looked up on `PATH`) |
| `HS_SCRIBE_OLMOCR_ENDPOINT` | `http://localhost:8081/v1` | OpenAI-compatible endpoint the CLI talks to (llama-swap) |
| `HS_SCRIBE_OLMOCR_MODEL` | `olmocr` | Model name the endpoint serves |
| `HS_SCRIBE_VRAM_HEADROOM_MB` | `15000` | Free VRAM needed to cold-start the model; below it (and with the model not resident) `/health` and `/readiness` refuse work |

## Models

| Model | Size | Purpose |
|---|---|---|
| [PP-DocLayout-V3](https://github.com/opendatalab/DocLayout-YOLO) | ~125 MB | Document layout detection. 25 region types with reading order. ONNX format, runs on CPU or CUDA. |
| [SLANet-Plus](https://github.com/PaddlePaddle/PaddleOCR) | ~8 MB | Table structure recognition. Detects cell boundaries in table regions. ONNX format. |
| [GLM-OCR](https://ollama.com/library/glm-ocr) | ~2.5 GB | Vision-language model for text extraction. Runs on Ollama with Metal (macOS) or CPU/CUDA (Linux). |

## Region types

PP-DocLayout-V3 detects 25 region classes. hs-scribe maps them to 6 processing types:

| Processing type | PP-DocLayout-V3 classes | Behavior |
|---|---|---|
| **Text** | text, paragraph_title, doc_title, abstract, content, reference, reference_content, footnote, vision_footnote, aside_text, vertical_text, figure_title, algorithm | OCR with `"OCR:"` prompt |
| **Table** | table | Structure detection + per-cell OCR, assembled as HTML |
| **Formula** | display_formula | OCR with `"Formula Recognition:"` prompt |
| **InlineFormula** | inline_formula | OCR with `"Formula Recognition:"` prompt |
| **Figure** | image, chart, seal | Skipped (placeholder in output) |
| **Skip** | header, footer, header_image, footer_image, number, formula_number | Omitted entirely |

### Repetition loops and gaps

A region whose VLM stream trips the repetition detector is aborted and left out of the page. The page continues, but the aborted region (a text region, or a single table cell, which is left empty) is counted as a skipped region, so the quality gate rejects the conversion as gapped and the tier chain escalates. The per-page diag keeps `repetition_aborted_regions` as its own counter.

Repetition cleanup (`clean_repetitions`) runs on a page only when its longest repeated run reaches the QC loop floor (128 bytes, the same floor `qc_verdict` uses). Below it the page is stored as the OCR text, so code indentation, wide table rules, dot leaders and `| - | - |` rows are never rewritten.

Table cell text from the VLM is HTML-escaped (`&`, `<`, `>`) before it goes into `<td>`.

HTML and EPUB chapters: every outermost `<article>` is converted, in document order (an `<article>` inside a sidebar `<aside>`, `<nav>` or similar is dropped with it). An `<aside>` that carries an `epub:type` attribute (EPUB 3 footnotes) is content and kept; other `<aside>` elements are dropped.

## Build

```sh
# Client library (used by the hs CLI)
cargo check -p hs-scribe

# --release builds need the tag the binary ships as (build-support/version.rs)
export HS_RELEASE_TAG=v0.0.1-rc.NNN

# Server binary
cargo build --release -p hs-scribe --features server --bin hs-scribe-server

# With CUDA
cargo build --release -p hs-scribe --features server,cuda --bin hs-scribe-server

# With evaluation harness (BLEU, TED, edit distance metrics)
cargo build --release -p hs-scribe --features eval --bin hs-scribe-server
```

The harness reports the official v1.5 overall (the mean of the text, TEDS and CDM averages) only when all three categories have scored pages; otherwise it prints "not available" and the JSON field is `null`. A dataset whose reference table has no parseable rows is rejected at load with an error naming the sample.

## Docker

Multi-arch images (amd64 + arm64) are published to GHCR on every release:

```sh
docker pull ghcr.io/home-still/hs-scribe-server:latest
docker run -p 7433:7433 -v ~/.home-still/models:/models:ro \
  -e HS_BACKEND_TOKEN="$HS_BACKEND_TOKEN" \
  -e HS_SCRIBE_OLLAMA_URL=http://host.docker.internal:11434 \
  ghcr.io/home-still/hs-scribe-server:latest
```

Or run the server natively with `hs serve scribe`.

### Health check

`/health` and `/readiness` are the only routes that need no token.

```sh
curl http://localhost:7433/health
# {"status":"ok","layout_model":true,"table_model":true}
```

### Streaming API

```sh
curl -X POST http://localhost:7433/scribe/stream \
  -H "Authorization: Bearer $HS_BACKEND_TOKEN" \
  -F 'pdf=@paper.pdf' \
  --no-buffer
# {"progress":{"stage":"parse","page":0,"total_pages":10,"message":"Parsed 10 pages"}}
# {"progress":{"stage":"layout","page":1,"total_pages":10,"message":"Layout done page 1/10"}}
# {"progress":{"stage":"vlm","page":1,"total_pages":10,"message":"OCR region 3/8 on page 1"}}
# ...
# {"result":{"markdown":"# Title\n\nContent..."}}
```

Max upload size: 256 MB. An upload that delivers no bytes for 60 s is refused with `408` and its slot is returned; a client that disconnects mid-conversion also frees its slot (and an `olmocr` run is killed).
