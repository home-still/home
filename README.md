# home-still

Free and open source tools to democratize knowledge acquisition, distillation, and comprehension.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
```

Installs the `hs` binary to `~/.local/bin/`. Supports macOS (Intel + Apple Silicon), Linux (x86_64 + ARM64), and Windows.

## Quick start

```sh
hs config init                                    # set up config + API keys
hs paper search "transformer attention mechanisms" # search across 6 providers
hs paper download "neural nets" -n 25              # download PDFs
hs scribe init                                     # set up PDF conversion server
hs scribe convert paper.pdf -o paper.md            # convert PDF to markdown
```

## What it does

home-still is a three-phase academic research engine:

| Phase | Tool | What it does | Status |
|---|---|---|---|
| **Acquire** | `hs paper` | Search and download papers from 6 providers | Working |
| **Distill** | `hs scribe` | Convert PDFs to markdown using layout detection + VLM OCR | Working |
| **Distill** | distill pipeline | Chunk, embed, and index 211M+ papers into Qdrant | Planned |
| **Comprehend** | vector search | Semantic search across the full academic corpus | Planned |

## Paper search

Search 6 academic providers simultaneously. Results are deduplicated, merged, and ranked using reciprocal rank fusion.

```sh
# Keyword search (queries all providers by default)
hs paper search "CRISPR gene editing"

# By author, limit to 5 results
hs paper search --type author "Hinton" -n 5

# Recent papers with abstracts
hs paper search "diffusion models" --date ">=2024" -a

# Sort by citations, minimum 100
hs paper search "attention is all you need" --sort citations --min-citations 100

# Single provider
hs paper search "neural ode" -p arxiv

# JSON output for scripting
hs paper search "LLM reasoning" --output json | jq '.papers[].title'
```

### Download

```sh
# Search and download matching papers
hs paper download "neural nets" -n 25

# Download a single paper by DOI
hs paper download --doi "10.48550/arXiv.2301.00001"

# Higher concurrency
hs paper download "transformers" -n 100 -c 8
```

Downloads show a progress bar with per-file status and ETA. Papers without download URLs are automatically filtered out.

### Search options

| Flag | Values | Default |
|---|---|---|
| `-t, --type` | keywords, title, author, doi, subject | keywords |
| `-p, --provider` | all, arxiv, openalex, semantic_scholar, europe_pmc, crossref, core | all |
| `-s, --sort` | relevance, date, citations | relevance |
| `-n, --max-results` | any number | 10 |
| `-d, --date` | `>=2024`, `>2023 <2025`, `>=2024-06` | none |
| `-a, --abstract` | show abstracts | off |
| `--min-citations` | minimum citation count | none |
| `--offset` | pagination offset | 0 |

### Providers

| Provider | Coverage | Rate limit |
|---|---|---|
| [arXiv](https://arxiv.org) | Physics, math, CS, biology preprints | 1 req / 3s |
| [OpenAlex](https://openalex.org) | 250M+ works across all disciplines | 10 req / s |
| [Semantic Scholar](https://semanticscholar.org) | 200M+ papers with citation graphs | 1 req / s |
| [Europe PMC](https://europepmc.org) | Biomedical and life sciences | 5 req / s |
| [CrossRef](https://crossref.org) | 147M+ DOI records | 10 req / s |
| [CORE](https://core.ac.uk) | 300M+ open access papers | 5 req / 10s |

All providers are queried in parallel when using `--provider all` (default). Results are deduplicated by DOI and fuzzy title matching, then ranked with reciprocal rank fusion.

## PDF-to-Markdown (Scribe)

Convert academic PDFs into structured markdown using a two-stage pipeline:

1. **Layout detection** -- PP-DocLayout-V3 via ONNX Runtime. Detects 10 region types: title, text, figures, tables, formulas, headers, footers, captions, references, equations.
2. **VLM OCR** -- Sends detected regions to a vision-language model (GLM-OCR via Ollama) for text extraction.
3. **Markdown assembly** -- Regions are ordered and assembled into section-aware markdown with heading hierarchy and table structure.

### Setup

```sh
hs scribe init    # downloads models, sets up Docker/Podman services
```

On **macOS Apple Silicon**, Ollama runs natively with Metal GPU acceleration (no Docker overhead). On **Linux with NVIDIA GPU**, CUDA acceleration is auto-detected. On **CPU-only** systems, inference runs in software mode.

### Convert

```sh
hs scribe convert paper.pdf              # output to stdout
hs scribe convert paper.pdf -o paper.md  # output to file
```

### Watch directory

Auto-convert PDFs as they appear in a directory:

```sh
hs scribe watch --dir ~/papers --output ~/papers/markdown
hs scribe watch   # watches current dir, outputs to ./markdown/
```

Skips PDFs that already have up-to-date markdown. Runs until CTRL+C.

### Server management

```sh
hs scribe server start   # start Docker services
hs scribe server stop    # stop Docker services
hs scribe server list    # show status and health
hs scribe server ping    # health check
```

## Configuration

```sh
hs config init          # creates ~/.home-still/config.yaml (interactive)
hs config show          # prints resolved config
hs config path          # prints config file path
```

Config file: `~/.home-still/config.yaml`

```yaml
paper:
  download_path: ~/Downloads/home-still/papers
  providers:
    openalex:
      # api_key: your-key-here
    semantic_scholar:
      # api_key: your-key-here
    core:
      # api_key: your-key-here
    crossref:
      # mailto: you@example.com
  download:
    # unpaywall_email: you@example.com  # enables more download sources

scribe:
  # Default output directory for converted markdown
  output_dir: ~/markdown
  # Directory to watch for new PDFs
  watch_dir: ~/papers
  # Scribe servers (PDFs are load-balanced across all servers)
  servers:
    - http://localhost:7433
    # - http://gpu-server:7433
    # - http://pi-cluster:7433
```

With multiple servers configured, `hs scribe watch` distributes PDFs across them in parallel, routing each to whichever server has the most available capacity. Each server exposes a `/readiness` endpoint so the client knows which ones can accept work.

Override with environment variables: `HOME_STILL_PAPER_DOWNLOAD_PATH=/tmp/papers`

## Global flags

Available on all commands:

| Flag | Values | Default |
|---|---|---|
| `--color` | auto, always, never | auto |
| `--output` | text, json, ndjson | text |
| `--quiet` | suppress non-result output | off |
| `--verbose` | debug-level output | off |
| `-y, --yes` | skip interactive prompts | off |

## Network ports

When running services across multiple machines, ensure these ports are open in your firewall:

| Port | Service | Direction | Notes |
|------|---------|-----------|-------|
| **7433** | Scribe server | Client → Server | PDF-to-markdown conversion |
| **7434** | Distill server | Client → Server | Embedding and semantic search |
| **6333** | Qdrant REST | Server internal | Vector DB HTTP API (used by distill server) |
| **6334** | Qdrant gRPC | Server internal | Vector DB gRPC (used by distill server) |
| **11434** | Ollama | Server internal | VLM for OCR (used by scribe server) |

**Client machines** (e.g., MacBook) need outbound access to ports 7433 and 7434 on the server.
**Server machines** (e.g., big) need ports 7433, 7434 open for inbound connections. Ports 6333, 6334, and 11434 are typically localhost-only.

Example (Linux firewall):
```sh
sudo ufw allow 7433/tcp   # scribe
sudo ufw allow 7434/tcp   # distill
```

Example (macOS):
```sh
# Allow incoming connections in System Settings > Network > Firewall
# Or add to /etc/pf.conf:
pass in proto tcp to port { 7433, 7434 }
```

## Architecture

```
crates/hs/          Unified CLI binary (hs paper, hs scribe, hs distill, hs status)
crates/hs-distill/  Vector embedding + semantic search (ONNX embeddings, Qdrant, client/server)
crates/hs-scribe/   PDF-to-markdown (ONNX layout detection + VLM OCR, client/server)
paper/              Academic paper meta-search library (6 providers, aggregation)
hs-common/          Shared infrastructure (reporter, service pool, catalog, compose)
```

## Build

```sh
cargo build --release -p hs                           # unified CLI
cargo check -p paper                                  # paper library
cargo check -p hs-scribe                              # scribe client
cargo check -p hs-scribe --features server            # scribe server
cargo test --workspace --exclude hs-scribe             # run tests
```

## License

MIT
