# hs-distill

Vector embedding and semantic search for academic papers. Chunks markdown documents, embeds them with BGE-M3 (ONNX), and stores them in Qdrant for similarity search.

## Architecture

```
Client (any machine)              Server (GPU/compute machine)
hs distill index ──HTTP──>  hs-distill-server
hs distill search ──HTTP──>       ├── chunk markdown
hs distill status ──HTTP──>       ├── embed (ONNX/fastembed)
                                  └── upsert ──> Qdrant
```

The client reads markdown files locally and sends content to the server over HTTP. The server handles embedding and Qdrant storage. This means you can index from a laptop over the network.

## Server setup

The server runs on the machine with compute resources (GPU or fast CPU). It needs Qdrant for vector storage.

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
```

This installs both `hs` and `hs-distill-server` to `~/.local/bin/`.

### 2. Configure

Add to `~/.home-still/config.yaml` on the server machine:

```yaml
distill_server:
  host: 0.0.0.0
  port: 7434
  qdrant_url: http://localhost:6334
  qdrant_data_dir: /path/to/data/qdrant   # where Qdrant stores vectors
  collection_name: academic_papers
```

All fields have defaults and are optional. The defaults are:

| Field | Default | Description |
|-------|---------|-------------|
| `host` | `0.0.0.0` | Bind address |
| `port` | `7434` | HTTP port |
| `qdrant_url` | `http://localhost:6334` | Qdrant gRPC endpoint |
| `qdrant_data_dir` | `{project_dir}/data/qdrant` | Qdrant storage on disk |
| `collection_name` | `academic_papers` | Qdrant collection name |
| `embedding.model` | `bge-m3` | Embedding model |
| `embedding.dimension` | `1024` | Vector dimension |
| `chunk_max_tokens` | `1000` | Max tokens per chunk |
| `chunk_overlap` | `100` | Token overlap between chunks |

Environment variable overrides use the `HS_DISTILL_` prefix (e.g., `HS_DISTILL_PORT=7434`).

### 3. Initialize Qdrant

```bash
hs distill init
```

This detects Docker/Podman, creates a compose file for Qdrant, pulls the image, and starts the container. Qdrant data is stored at the configured `qdrant_data_dir`.

To recreate the compose config (e.g., after changing `qdrant_data_dir`):

```bash
hs distill init --force
```

### 4. Start

```bash
hs distill server start
```

Starts both the Qdrant container and the native `hs-distill-server` process. Logs are written to `{project_dir}/logs/distill-server.log`.

### 5. Manage

```bash
hs distill server stop     # stop both Qdrant and distill server
hs distill server ping     # health check
hs distill status          # show Qdrant health, server PID, collection info
```

## Client setup

The client can run on any machine that can reach the server over the network.

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
```

### 2. Configure

Add to `~/.home-still/config.yaml` on the client machine:

```yaml
distill:
  servers:
    - http://<server-ip>:7434
  markdown_dir: /path/to/markdown    # local path to markdown files
  catalog_dir: /path/to/catalog      # local path to catalog YAMLs
```

| Field | Default | Description |
|-------|---------|-------------|
| `servers` | `["http://localhost:7434"]` | Distill server URL(s) |
| `markdown_dir` | `{project_dir}/markdown` | Where to find `.md` files |
| `catalog_dir` | `{project_dir}/catalog` | Where to find catalog `.yaml` files |

### 3. Index

```bash
hs distill index                        # index all markdown files
hs distill index --file doc1.md doc2.md # index specific files
```

The client reads each `.md` file locally and sends its content to the server for chunking, embedding, and storage. Files are identified by their stem name (e.g., `paper.md` becomes doc_id `paper`).

### 4. Search

```bash
hs distill search "transformer attention mechanism"
hs distill search "neural networks" --limit 20
hs distill search "deep learning" --year ">2020" --topic "nlp"
```

### 5. Status

```bash
hs distill status
```

Shows Qdrant health, server status, collection name, point count, and document count.

## Pipeline

Each markdown file goes through:

1. **Chunking** -- split at sentence boundaries with configurable max tokens and overlap. Page-aware (respects `---` page separators from scribe).
2. **Metadata extraction** -- pulls title, authors, DOI, year from catalog YAML + regex patterns. Optional LLM extraction for keywords/topics via Ollama.
3. **Embedding** -- BGE-M3 via ONNX (fastembed). 1024-dimensional dense vectors.
4. **Qdrant upsert** -- deterministic point IDs (xxhash + UUID v5) enable idempotent re-indexing. Rich payload with full metadata for filtered search.

## API endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Server status and compute device |
| `/readiness` | GET | Ready status + in-flight request count |
| `/status` | GET | Collection stats (points, documents, device) |
| `/distill` | POST | Index a document (non-streaming) |
| `/distill/stream` | POST | Index with NDJSON streaming progress |
| `/search` | POST | Semantic search with optional filters |

## Building from source

```bash
# Client only (lightweight, no ONNX deps)
cargo build --release -p hs-distill

# Server (requires ONNX runtime)
cargo build --release -p hs-distill --features server
```
