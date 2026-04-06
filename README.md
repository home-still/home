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
hs distill init                                    # set up vector search
hs distill index                                   # index markdown into Qdrant
hs distill search "attention mechanism"            # semantic search
hs status                                          # live pipeline dashboard
```

## What it does

home-still is a four-phase academic research engine:

| Phase | Tool | What it does | Status |
|---|---|---|---|
| **Acquire** | `hs paper` | Search and download papers from 6 providers | Working |
| **Convert** | `hs scribe` | Convert PDFs to markdown using layout detection + VLM OCR | Working |
| **Index** | `hs distill` | Chunk, embed, and index documents into Qdrant | Working |
| **Search** | `hs distill search` | Semantic search across indexed documents | Working |
| **Cloud** | `hs cloud` | Secure remote access via Cloudflare tunnel + OAuth2 | Working |

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

Converted output is post-processed with repetition detection to clean VLM generation loops.

### Setup

```sh
hs scribe init    # downloads models, sets up Docker/Podman services
```

On **macOS Apple Silicon**, Ollama runs natively with Metal GPU acceleration. On **Linux with NVIDIA GPU**, CUDA acceleration is auto-detected. On **CPU-only** systems, inference runs in software mode.

### Convert

```sh
hs scribe convert paper.pdf              # output to stdout
hs scribe convert paper.pdf -o paper.md  # output to file
```

### Watch directory

Auto-convert PDFs as they appear in a directory:

```sh
hs scribe watch start   # start background daemon
hs scribe watch stop    # stop daemon
hs scribe status        # show conversion progress
```

### Server management

```sh
hs scribe server start   # start Docker services
hs scribe server stop    # stop Docker services
hs scribe server list    # show status and health
hs scribe server ping    # health check
```

With multiple servers configured, PDFs are load-balanced across them based on server readiness. See [crates/hs-scribe/README.md](crates/hs-scribe/README.md) for full documentation.

## Semantic Search (Distill)

Chunk, embed, and index converted markdown into a Qdrant vector database for semantic search.

### Setup

```sh
hs distill init                          # set up Qdrant container
hs distill server start                  # start Qdrant + distill server
```

### Index and search

```sh
hs distill index                         # index all markdown files
hs distill search "attention mechanism"  # semantic search
hs distill search "neural nets" --year ">2020" --limit 5
hs distill status                        # collection stats
```

Search results include title, authors, year, relevance score, and text snippets. Low-quality chunks (repetition loops, garbled text) are automatically filtered during indexing.

Output formats: `--output text` (default), `--output json`, `--output ndjson`.

See [crates/hs-distill/README.md](crates/hs-distill/README.md) for full documentation.

## Status Dashboard

```sh
hs status
```

Live TUI showing:
- **Pipeline stats** -- PDF count, markdown count, catalog entries, embedded documents/chunks
- **Service health** -- scribe servers (with version + in-flight conversions), distill servers, Qdrant
- **Recent conversions** -- title, pages, duration, timestamp

Refreshes every 3 seconds. Press `q` to quit.

## Self-Update

```sh
hs upgrade              # download latest, update containers, health check
hs upgrade --check      # check for updates without installing
hs upgrade --force      # reinstall even if already on latest
```

Downloads the correct binary for your platform from GitHub releases, replaces the running binary via atomic rename, pulls new Docker images, restarts containers, and runs health checks.

## Cloud Access

Access your home-still services from anywhere over the internet. Uses a Cloudflare tunnel for connectivity and OAuth2 for authentication.

### Architecture

```
Remote machine              Cloudflare Edge              Gateway host          LAN services
  hs CLI / Claude ──[HTTPS]──> cloud.example.com ──[QUIC]──> hs-gateway ──[HTTP]──> scribe, distill
```

### Prerequisites

- A host on your LAN running [cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/) with an active tunnel
- A domain name managed by Cloudflare (e.g., `cloud.example.com`)

### Gateway setup

On your tunnel host:

```sh
hs cloud init                # generate signing secret

# Edit ~/.home-still/config.yaml:
cloud:
  role: gateway
  gateway_url: https://cloud.example.com
  gateway:
    listen: 127.0.0.1:7440
    routes:
      scribe: http://gpu-server:7433
      distill: http://gpu-server:7434
      mcp: http://127.0.0.1:7445
```

Add an ingress rule to your cloudflared config:

```yaml
- hostname: cloud.example.com
  service: http://127.0.0.1:7440
```

Start the gateway as a systemd service or manually:

```sh
hs-gateway --gateway-url https://cloud.example.com
```

### Enrolling devices

On the gateway host:

```sh
hs cloud invite              # generates a 5-minute, single-use code
```

On the remote machine:

```sh
hs cloud enroll --gateway https://cloud.example.com
# enter the enrollment code when prompted
```

### Claude Desktop (OAuth2)

The gateway implements OAuth 2.1 Authorization Code + PKCE, which Claude Desktop uses for remote MCP servers:

1. Add `https://cloud.example.com/mcp` as a remote MCP server in Claude Desktop
2. Claude opens your browser to the gateway's authorization page
3. Generate a code with `hs cloud invite` on the gateway host
4. Enter the code in the browser form
5. Claude Desktop stores tokens and auto-refreshes them

### CLI access

For scripts and CLI tools:

```sh
hs cloud token               # prints a 4-hour access token
hs cloud status               # check connection and token validity
```

See [crates/hs-gateway/README.md](crates/hs-gateway/README.md) for full documentation.

## MCP Server

home-still includes a [Model Context Protocol](https://modelcontextprotocol.io) server exposing the full read API as 13 tools:

| Tool | Description |
|------|-------------|
| `paper_search` | Search academic papers across 6 providers |
| `paper_get` | Look up a paper by DOI |
| `catalog_list` | List all papers with conversion status |
| `catalog_read` | Read full catalog metadata for a paper |
| `markdown_list` | List converted markdown documents |
| `markdown_read` | Read a markdown document (full or by page) |
| `scribe_health` | Check scribe server status |
| `scribe_convert` | Convert a PDF to markdown |
| `distill_search` | Semantic search across indexed documents |
| `distill_status` | Qdrant collection statistics |
| `distill_exists` | Check if a document is indexed |
| `system_status` | Full pipeline health and stats |

### Local (stdio)

For Claude Desktop or Claude Code on the same machine:

```json
{
  "mcpServers": {
    "home-still": {
      "command": "hs-mcp"
    }
  }
}
```

### Remote (SSE via gateway)

Run the MCP server on your LAN and expose through the gateway:

```sh
hs-mcp --serve 127.0.0.1:7445
```

Remote clients connect via `https://cloud.example.com/mcp` using OAuth2. See [crates/hs-mcp/README.md](crates/hs-mcp/README.md) for full documentation.

## Configuration

```sh
hs config init          # creates ~/.home-still/config.yaml (interactive)
hs config show          # prints resolved config
hs config path          # prints config file path
```

Config file: `~/.home-still/config.yaml`

```yaml
home:
  project_dir: ~/home-still           # papers, markdown, catalog

paper:
  providers:
    openalex:
      # api_key: your-key-here
  download:
    # unpaywall_email: you@example.com

scribe:
  servers:
    - http://localhost:7433
    # - http://gpu-server:7433

distill:
  servers:
    - http://localhost:7434

cloud:
  role: client                         # or "gateway" on the tunnel host
  gateway_url: https://cloud.example.com
```

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

| Port | Service | Direction | Notes |
|------|---------|-----------|-------|
| **7433** | Scribe server | Client -> Server | PDF-to-markdown conversion |
| **7434** | Distill server | Client -> Server | Embedding and semantic search |
| **7440** | Gateway | Tunnel -> Gateway | Cloud access reverse proxy |
| **7445** | MCP server | Gateway -> MCP | Model Context Protocol (SSE) |
| **6333** | Qdrant REST | Server internal | Vector DB HTTP API |
| **6334** | Qdrant gRPC | Server internal | Vector DB gRPC |
| **11434** | Ollama | Server internal | VLM for OCR |

## Architecture

```
crates/hs/          Unified CLI binary (paper, scribe, distill, status, upgrade, cloud, config)
crates/hs-scribe/   PDF-to-markdown (ONNX layout detection + VLM OCR, client/server)
crates/hs-distill/  Vector embedding + semantic search (ONNX embeddings, Qdrant, client/server)
crates/hs-gateway/  Cloud access reverse proxy (OAuth2, token auth, service routing)
crates/hs-mcp/      MCP server (13 tools, stdio + SSE transport)
paper/              Academic paper meta-search library (6 providers, aggregation)
hs-common/          Shared infrastructure (reporter, service pool, catalog, auth, compose)
```

## Build

```sh
cargo build --release -p hs                           # unified CLI
cargo build --release -p hs-gateway                   # cloud gateway
cargo build --release -p hs-mcp                       # MCP server
cargo check -p hs-scribe --features server            # scribe server
cargo check -p hs-distill --features server           # distill server
cargo test --workspace                                # run tests
```

## License

MIT
