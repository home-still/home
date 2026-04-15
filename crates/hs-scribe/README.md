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
# One-time setup (downloads models, starts services)
hs scribe init

# Convert a PDF
hs scribe convert paper.pdf -o paper.md

# Watch a folder and auto-convert new PDFs
hs scribe watch --dir ~/papers --output ~/papers/markdown
```

## Setup

```sh
hs scribe init
```

This single command handles everything:

1. **Detects your container runtime** (Docker or Podman). On macOS, auto-installs via Homebrew if needed.
2. **Detects your hardware**:
   - Apple Silicon? Installs Ollama natively for Metal GPU acceleration.
   - NVIDIA GPU? Enables CUDA in the container config.
   - Neither? Falls back to CPU mode.
3. **Downloads the layout model** (~125 MB ONNX file).
4. **Writes the Docker Compose config** tailored to your platform.
5. **Starts services and pulls the VLM model** (~2.5 GB on first run).

Use `hs scribe init --force` to regenerate everything, or `hs scribe init --check` for a dry-run status report.

### Platform behavior

| Platform | VLM runs on | GPU acceleration |
|---|---|---|
| macOS Apple Silicon | Host (native Ollama) | Metal GPU |
| Linux + NVIDIA | Container (Ollama) | CUDA |
| Linux / macOS Intel | Container (Ollama) | CPU only |

On Apple Silicon, the scribe server runs in a container but connects back to Ollama on the host via `host.docker.internal:11434`. This gives the VLM access to Metal, which is dramatically faster than CPU inference inside a Linux VM.

## Commands

### Convert a PDF

```sh
hs scribe convert paper.pdf              # markdown to stdout
hs scribe convert paper.pdf -o paper.md  # markdown to file
hs scribe convert paper.pdf --server http://remote:7433  # use a remote server
```

During conversion, you'll see a live progress bar with elapsed time and ETA:

```
Converting ━━━━━━━━━━━━━━╸              22/43  00:01:30 ETA 00:00:42  [vlm] OCR region 5/12 on page 22
```

### Watch a directory

```sh
hs scribe watch --dir ~/papers --output ~/papers/markdown
hs scribe watch   # uses watch_dir/output_dir from config, or current dir
```

Watches recursively for new or modified `.pdf` files. Skips PDFs that already have up-to-date markdown (compares file modification times). Runs until CTRL+C.

### Manage the server

```sh
hs scribe server start   # start containers
hs scribe server stop    # stop containers
hs scribe server list    # show status + health
hs scribe server ping    # quick health check
hs scribe server ping http://remote:7433  # check a specific server
```

## Architecture

```
hs scribe convert paper.pdf
    |
    v
ScribeClient (HTTP multipart upload to localhost:7433)
    |
    v
hs-scribe-server (Docker container)
    |--- PDF rendering (PDFium, configurable DPI)
    |--- Layout detection (ONNX: PP-DocLayout-V3, 25 region types)
    |--- Table structure (ONNX: SLANet-Plus, cell boundaries)
    |--- VLM OCR (Ollama / OpenAI-compat / Cloud)
    |         |
    |         +-- Metal GPU on macOS (native Ollama)
    |         +-- CUDA on Linux (containerized Ollama)
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

With multiple servers, `hs scribe convert` and `hs scribe watch` automatically load-balance across them. The CLI queries each server's `/readiness` endpoint and routes each PDF to the server with the most available VLM slots.

Server discovery uses the gateway service registry when available, falling back to the configured server list.

### Server config (environment variables)

Server-side settings use environment variables with the `HS_SCRIBE_` prefix. They can also be set in `~/.config/home-still/config.yaml`.

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
| `HS_SCRIBE_TIMEOUT_SECS` | `120` | VLM request timeout |

### Performance tuning

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_VLM_CONCURRENCY` | `4` | Max concurrent VLM requests across pages |
| `HS_SCRIBE_REGION_PARALLEL` | `4` | Max concurrent regions within one page |
| `HS_SCRIBE_PARALLEL` | `1` | Max concurrent pages in FullPage mode |
| `HS_SCRIBE_USE_CUDA` | `true` | Enable CUDA for ONNX layout detection |
| `HS_SCRIBE_MAX_IMAGE_DIM` | `1800` | Downscale images larger than this (pixels) |

### Model paths

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_LAYOUT_MODEL_PATH` | `pp-doclayoutv3.onnx` | PP-DocLayout-V3 ONNX model |
| `HS_SCRIBE_TABLE_MODEL_PATH` | `slanet-plus.onnx` | SLANet-Plus ONNX model |

Model paths are resolved relative to `~/.local/share/home-still/models/` if not absolute.

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

## Build

```sh
# Client library (used by the hs CLI)
cargo check -p hs-scribe

# Server binary
cargo build --release -p hs-scribe --features server --bin hs-scribe-server

# With CUDA
cargo build --release -p hs-scribe --features server,cuda --bin hs-scribe-server

# With evaluation harness (BLEU, TED, edit distance metrics)
cargo build --release -p hs-scribe --features eval --bin hs-scribe-server
```

## Docker

Multi-arch images (amd64 + arm64) are published to GHCR on every release:

```sh
docker pull ghcr.io/home-still/hs-scribe-server:latest
docker run -p 7433:7433 -v ~/.local/share/home-still/models:/models:ro \
  -e HS_SCRIBE_OLLAMA_URL=http://host.docker.internal:11434 \
  ghcr.io/home-still/hs-scribe-server:latest
```

Or just use `hs scribe init` which handles all of this automatically.

### Health check

```sh
curl http://localhost:7433/health
# {"status":"ok","layout_model":true,"table_model":true}
```

### Streaming API

```sh
curl -X POST http://localhost:7433/scribe/stream \
  -F 'pdf=@paper.pdf' \
  --no-buffer
# {"progress":{"stage":"parse","page":0,"total_pages":10,"message":"Parsed 10 pages"}}
# {"progress":{"stage":"layout","page":1,"total_pages":10,"message":"Layout done page 1/10"}}
# {"progress":{"stage":"vlm","page":1,"total_pages":10,"message":"OCR region 3/8 on page 1"}}
# ...
# {"result":{"markdown":"# Title\n\nContent..."}}
```

Max upload size: 256 MB.
