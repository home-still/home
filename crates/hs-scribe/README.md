# hs-scribe

PDF-to-markdown converter using ONNX layout detection and vision-language model OCR. Part of [home-still](../../README.md).

## How it works

1. **Layout detection** -- PP-DocLayout-V3 via ONNX Runtime classifies page regions into 10 types: title, plain text, figures, tables, formulas, headers, footers, captions, references, equations.
2. **Table structure recognition** -- SLANet-Plus detects table cell boundaries for structured table extraction.
3. **VLM OCR** -- Detected regions and table cells are sent to a vision-language model (GLM-OCR) for text extraction.
4. **Markdown assembly** -- Regions are ordered by reading position and assembled into section-aware markdown with heading hierarchy, figure/table captions, and HTML tables.

## Architecture

Client-server model:
- **Server** (`hs-scribe-server`): Runs in a Docker/Podman container. Hosts the ONNX models and communicates with the VLM backend (Ollama). Streams NDJSON progress events during processing.
- **Client** (in `hs` CLI): Sends PDFs to the server, receives streaming progress and the final markdown.

```
hs scribe convert paper.pdf
    |
    v
ScribeClient (HTTP multipart upload)
    |
    v
hs-scribe-server (Docker container)
    |--- PDF parsing (pdfium)
    |--- Layout detection (ONNX: PP-DocLayout-V3)
    |--- Table structure (ONNX: SLANet-Plus)
    |--- VLM OCR (Ollama: GLM-OCR)  <-- Metal GPU on macOS
    |
    v
NDJSON progress stream -> markdown result
```

## Setup

```sh
hs scribe init    # auto-detects platform, downloads models, starts services
```

### Platform-specific behavior

| Platform | VLM Backend | GPU |
|---|---|---|
| macOS Apple Silicon | Native Ollama (installed via Homebrew) | Metal GPU |
| Linux + NVIDIA | Containerized Ollama | CUDA |
| Linux / macOS Intel | Containerized Ollama | CPU only |

On Apple Silicon, `hs scribe init` installs Ollama natively so it can use Metal GPU acceleration. The scribe server still runs in a container but connects to the host Ollama via `host.docker.internal`.

## VLM backends

| Backend | Config | Use case |
|---|---|---|
| Ollama | `HS_SCRIBE_BACKEND=Ollama` | Default. Local inference via Ollama |
| OpenAI-compatible | `HS_SCRIBE_BACKEND=OpenAi` | vLLM, sglang, MLX serving |
| Cloud | `HS_SCRIBE_BACKEND=Cloud` | Remote API providers |

## Commands

```sh
# Convert a single PDF
hs scribe convert paper.pdf -o paper.md

# Watch a directory for new PDFs
hs scribe watch --dir ~/papers --output ~/papers/markdown

# Server management
hs scribe server start
hs scribe server stop
hs scribe server list
hs scribe server ping
```

## Configuration

Environment variables (prefix `HS_SCRIBE_`):

| Variable | Default | Description |
|---|---|---|
| `HS_SCRIBE_BACKEND` | Ollama | VLM backend choice |
| `HS_SCRIBE_OLLAMA_URL` | http://localhost:11434 | Ollama server URL |
| `HS_SCRIBE_MODEL` | glm-ocr:latest | VLM model name |
| `HS_SCRIBE_DPI` | 200 | PDF rendering DPI |
| `HS_SCRIBE_VLM_CONCURRENCY` | 4 | Concurrent VLM requests |
| `HS_SCRIBE_PIPELINE_MODE` | PerRegion | PerRegion or FullPage |
| `HS_SCRIBE_USE_CUDA` | true | Enable CUDA if available |
| `HS_SCRIBE_MAX_IMAGE_DIM` | 1800 | Max image dimension for downscaling |

## Progress reporting

The server streams NDJSON progress events during conversion:
- `[parse]` -- PDF page extraction
- `[layout]` -- Per-page layout detection
- `[vlm]` -- Per-region and per-table-cell OCR with counts
- `[done]` -- Final markdown assembly

The CLI displays these as a progress bar with elapsed time and ETA.

## Models

| Model | Size | Purpose |
|---|---|---|
| PP-DocLayout-V3 | ~125 MB | Document layout detection (ONNX) |
| SLANet-Plus | ~8 MB | Table structure recognition (ONNX) |
| GLM-OCR | ~2.5 GB | Vision-language model for OCR (Ollama) |

## Build

```sh
# Client only (used by hs CLI)
cargo check -p hs-scribe

# Server (requires server feature)
cargo build --release -p hs-scribe --features server --bin hs-scribe-server

# With CUDA support
cargo build --release -p hs-scribe --features server,cuda --bin hs-scribe-server

# With evaluation harness
cargo build --release -p hs-scribe --features eval --bin hs-scribe-server
```

## Docker

The server image is published to `ghcr.io/home-still/hs-scribe-server:latest` for both amd64 and arm64.

```sh
# Pull and run manually
docker pull ghcr.io/home-still/hs-scribe-server:latest
docker run -p 7432:7432 -v /path/to/models:/models:ro ghcr.io/home-still/hs-scribe-server:latest
```

Or use `hs scribe init` which sets everything up automatically.
