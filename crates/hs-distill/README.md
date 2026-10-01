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

All fields have defaults and are optional. The server refuses to start on an invalid configuration (it names the key). The defaults are:

| Field | Default | Description |
|-------|---------|-------------|
| `host` | `0.0.0.0` | Bind address (the `--host` flag wins) |
| `port` | `7434` | HTTP port (the `--port` flag wins; `hs serve distill --port` sets `HS_DISTILL_PORT`) |
| `qdrant_url` | `http://localhost:6334` | Qdrant gRPC endpoint |
| `qdrant_data_dir` | `{project_dir}/data/qdrant` | Qdrant storage on disk |
| `collection_name` | `academic_papers` | Default Qdrant collection |
| `collections` | `[paper_abstracts, personal_docs]` | Further collections requests may name. Every configured collection is created/verified at startup; a request naming any other collection gets HTTP 400 and nothing is created. |
| `embedding.dimension` | `1024` | Expected vector width. Checked against what the model actually returns at startup; a mismatch is a startup error. |
| `embedding.max_length` | `1280` | Tokens the model sees per text (longer input is truncated by the tokenizer). `chunk_max_tokens + 48` must fit. Raising it costs VRAM per batch, so `embedding.batch_size` is capped at `128 x 512^2 / max_length^2` rows (32 rows at 1024, 20 at 1280). Max 8192. |
| `embedding.batch_size` | `32` (capped) | Rows per forward pass; at least 1 |
| `embedding.pool_size` | `1` | Model copies; at least 1 |
| `hnsw.m` / `hnsw.ef_construct` | `16` / `100` | HNSW graph of **new** collections (`m: 0` is rejected) |
| `hnsw.search_ef` | `128` | Candidates considered per query |
| `chunk_max_tokens` | `1000` | Max tokens per chunk |
| `chunk_overlap` | `100` | Token overlap between chunks (must be smaller than `chunk_max_tokens`) |
| `llm_metadata` | `false` | Extract keywords/topics with Ollama (`ollama_url` incl. port, `metadata_model`, `ollama_timeout_secs: 120`); a failed call fails that document |

Removed keys `embedding.model` and `embedding.sparse_enabled` have no effect (the model is fixed and embeddings are dense-only); the server logs a warning if they are still set.

**Collections created before the HNSW settings existed were built with HNSW off (`m: 0`)**: every query is a brute-force scan. The server reports such a collection at ERROR level on every start and does not change it, because enabling HNSW makes Qdrant index the whole corpus. Vectors indexed before `embedding.max_length` existed were truncated at 512 tokens and need a re-embed to cover the tail of each chunk.

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
| `servers` | `["http://localhost:7434"]` | Distill server URL(s); overridden by gateway registry when available |
| `markdown_dir` | `{project_dir}/markdown` | Where to find `.md` files |
| `catalog_dir` | `{project_dir}/catalog` | Where to find catalog `.yaml` files |
| `index_timeout_secs` | `1800` | Deadline for one indexing request. With NATS events it must be at least 120 s below `events.nats.ack_wait_secs` (default 7200). |

Server discovery uses the gateway service registry when available, falling back to the configured server list.

### 3. Index

```bash
hs distill index                        # index all markdown files
hs distill index --file doc1.md doc2.md # index specific files
```

The client reads each `.md` file locally and sends its content to the server for chunking, embedding, and storage. Files are identified by their stem name (e.g., `paper.md` becomes doc_id `paper`). The server never reads documents from disk: a request without `content` is rejected with HTTP 400. Re-indexing a document replaces all of its chunks (including ones past the new end), and a document that is skipped (empty, an anti-bot stub, nothing passes the quality filter) has its old chunks removed.

Indexing can also be triggered automatically: `hs scribe watch` auto-starts the distill indexer when new conversions complete, so newly converted markdown is embedded without a separate manual step.

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
3. **Embedding** -- BGE-M3 via ONNX (fastembed) on CUDA. 1024-dimensional dense vectors. A panic inside ONNX Runtime poisons its model slot; `/health` reports it and the process exits so the supervisor restarts it.
4. **Qdrant upsert** -- deterministic point IDs (xxhash + UUID v5) enable idempotent re-indexing; chunks past the document's new end are deleted after the upsert succeeds. Rich payload with full metadata for filtered search.

## API endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | 200 with server status; 503 if the embedder is unusable or Qdrant is unreachable |
| `/readiness` | GET | `ready` (embedder healthy and Qdrant answering), `capacity` (embedder slots), `in_flight`, `reason`; 503 when not ready |
| `/status` | GET | Collection stats (points, documents, device); `documents_count_truncated` if the count hit 1,000,000 |
| `/distill` | POST | Index a document (non-streaming). `content` is required |
| `/distill/stream` | POST | Index with NDJSON streaming progress. `content` is required |
| `/search` | POST | Semantic search with optional filters. `limit` defaults to 10 and is clamped to 200; an unparseable `year` filter is a 400 |
| `/exists/{doc_id}`, `/docs` | GET | Per-document chunk count; distinct doc ids (`limit` up to 1,000,000, larger is a 400; `truncated` flags a partial list) |
| `/doc/{doc_id}`, `/collection/reset`, `/scrub-interstitials` | DELETE / POST | Destructive maintenance. **Unauthenticated** (RA-26 auth pending): do not expose the port beyond trusted hosts |

Client errors (bad input, unknown collection) are HTTP 400; a missing dependency is 503; anything else is 500.

## Building from source

```bash
# Client only (lightweight, no ONNX deps)
cargo build --release -p hs-distill

# Server (requires ONNX runtime)
cargo build --release -p hs-distill --features server
```
