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

## How search works

Search runs two channels over the same collection and fuses their
**ranks** with Reciprocal Rank Fusion:

- **Dense** — the BGE-M3 vector of the query, cosine similarity. Finds
  passages that *mean* what the query means.
- **Lexical** — a Qdrant text index over the chunk text. Finds passages
  that *say* the query's words.

The two are not redundant. A dense embedder matches "thyroid" generically
and loses "Hashimoto"; a keyword match cannot answer "how does attention
work". Fusing ranks (not scores — a cosine and a term count are not on
the same scale) keeps what each channel is good at.

The lexical channel needs a text index on `chunk_text`, which the server
creates at startup like every other required index.

Which terms the lexical channel requires is decided by how rare they are
in the corpus, because Qdrant's text match is an AND and a real question
is not an AND: "Hashimoto thyroiditis euthyroid" matches no single
chunk, though the passage is in the library. The two rarest terms are
required, falling back to one — measured recall@10 of 25/25 on
`docs/loop/queries.yaml`, against 20/25 for the dense channel alone.

**Reference lists are demoted.** A bibliography entry matches a query
about a topic perfectly well in a dense embedding — semantically, a
citation to work about hippocampal neurogenesis *is* about hippocampal
neurogenesis — so a chunk of `doi:` lines can be the best-ranked hit for
a question. Such chunks are pushed below the passages that discuss the
topic; they are not dropped, so a user asking for a bibliography still
gets it, and a page with no prose at all still leads with the reference
list (sometimes the only chunk in the corpus that mentions a term is a
citation of it). The detection is structural — a citation line carries a
`doi:`, a parenthesised year after author names, `et al.`, a page
range, a volume, or a numbered-list marker — and is deliberately
conservative: a chunk that mixes a citation with real prose discusses
the topic and cites it, and is not demoted.

## Server setup

The server runs on the machine with the GPU. It needs an NVIDIA GPU with CUDA (there is no CPU path: the server refuses to start when the model does not land on the GPU) and Qdrant for vector storage.

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
| `embedding.adaptive_batch` | `true` | Tune the batch size in-process against observed throughput, starting from `batch_size`. A batch the accelerator **cannot allocate** also tunes it — down, not up: the card is busy, and a smaller batch is the answer |
| `embedding.idle_release_secs` | unset | Drop the model from GPU memory after this many idle seconds (at least 1); the next request reloads it (~10 s). Unset keeps it resident |
| `embedding.vram_floor_mb` | `5000` | Free VRAM required before (re)loading the model; below it the load fails naming the GPU holders |
| `embedding.gpu_yield_file` | unset | The host GPU broker's yield file (`{"for","since","expires"}`). While it names an unexpired tenant a (re)load fails with `gpu busy: yielded to <tenant>`, so the event is NAK'd and redelivered |
| `embedding.gpu_make_room_cmd` | unset | Argv run once (with `--need-mib <vram_floor_mb>` appended) when a (re)load is blocked; exit 0 means "try again now" |
| `hnsw.m` / `hnsw.ef_construct` | `16` / `100` | HNSW graph of **new** collections (`m: 0` is rejected) |
| `hnsw.search_ef` | `128` | Candidates considered per query |
| `hnsw.max_indexing_threads` | `4` | Index-build threads used by `POST /collection/hnsw` (1..=64) |
| `chunk_max_tokens` | `1000` | Max tokens per chunk |
| `chunk_overlap` | `100` | Token overlap between chunks (must be smaller than `chunk_max_tokens`) |
| `qdrant_upsert_batch` / `qdrant_upsert_parallelism` | `1000` / `4` | Chunks per Qdrant upsert request / requests in flight per document; at least 1 each |
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
hs serve distill start
```

Starts both the Qdrant container and the native `hs-distill-server` process. Logs are written to `{project_dir}/logs/distill-server.log`.

### 5. Manage

```bash
hs serve distill stop     # stop the distill server
hs status                 # health of the configured distill servers
hs distill status          # show Qdrant health, server liveness + PID, collection info
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
| `servers` | none | Distill server URL(s), e.g. `[http://<host>:7434]`. There is no default: a command that needs a server fails naming this key |
| `markdown_dir` | `{project_dir}/markdown` | Where to find `.md` files |
| `catalog_dir` | `{project_dir}/catalog` | Where to find catalog `.yaml` files |
| `index_timeout_secs` | `1800` | Deadline for one indexing request. The event watcher heartbeats the broker while it runs, so it is independent of `events.nats.ack_wait_secs`. |
| `concurrency` | hardware default | Documents `hs distill watch-events` indexes in parallel; at least 1 |

The configured `distill.servers` list is the only source of server addresses; there is no service registry.

### 3. Index

```bash
hs distill index                        # index all markdown files
hs distill index --file doc1.md doc2.md # index specific files
```

The client reads each `.md` file locally and sends its content to the server for chunking, embedding, and storage. Files are identified by their stem name (e.g., `paper.md` becomes doc_id `paper`). The server never reads documents from disk: a request without `content` is rejected with HTTP 400. Re-indexing a document replaces all of its chunks (including ones past the new end), and a document that is skipped (empty, an anti-bot stub, nothing passes the quality filter) has its old chunks removed.

Indexing is also triggered automatically: `hs distill watch-events` (`hs serve distill-watch --install` runs it as a service) indexes each markdown object when a `scribe.completed` event arrives, so newly converted markdown is embedded without a separate manual step.

### Repairing missing titles

A document's title is copied from its catalog row into the Qdrant payload
at index time, and a search hit reads it back out of the payload. A
document indexed *before* its catalog row existed was therefore indexed
with no title, and — because the corpus is not re-indexed — it stays that
way.

```bash
hs distill backfill-titles               # dry run: report what it would write
hs distill backfill-titles --apply        # write it
```

The title comes from the catalog where the catalog has one, and from the
document's own text — its first heading, or its first plain line —
where it does not. A document whose stored title is a converter artifact
(the OCR emitted a table or a repeated line where the title page should
be) has it replaced by the catalog's.

The write is a **payload update**: the vector is untouched, so this is
not a re-embed and needs no GPU and no embedding model. Documents that
neither source can title are reported by name, never given a placeholder
that would look like a title.

### Paper abstracts

```bash
hs distill abstracts build              # embed entries without an abstract_embed stamp
hs distill abstracts build --force      # re-extract and re-embed every entry
hs distill abstracts reconcile          # same as build without --force (new papers only)
hs distill abstracts status             # coverage by source (openalex / catalog / markdown)
```

Each paper's abstract comes from the OpenAlex catalog, then the catalog-stored abstract, then a heuristic extraction from the converted markdown. The markdown extractor ends an abstract at a markdown heading that starts with an IMRaD label (`## Results and Discussion`), at a **standalone** IMRaD label line (`Introduction`, `Background`, `Methods`, `Materials and Methods`, `Results`, `Discussion`, `Conclusion(s)` — optionally with `**` markers and a trailing `:`), or at a line starting `Keywords`/`References`/`Funding`/`Acknowledgements`. A structured abstract's own `Background: …` / `Results show …` lines no longer end it.

`build` and `reconcile` skip entries that already carry an `abstract_embed` stamp, so abstracts that were truncated by the earlier, looser section-break rule are only repaired by `hs distill abstracts build --force`, which re-extracts and re-embeds every entry.

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
2. **Metadata extraction** -- title, authors, DOI, publication date and the other fields come from the catalog entry sent with the request; DOI and year are never regex-extracted from the body text (the first DOI or year in a paper is usually a citation). Without a catalog entry the chunks carry none of them. Optional LLM extraction for keywords/topics via Ollama.
3. **Embedding** -- BGE-M3 via ONNX (fastembed) on CUDA. 1024-dimensional dense vectors. A panic inside ONNX Runtime poisons its model slot; `/health` reports it and the process exits so the supervisor restarts it.

   **On a shared card**, the batch is sized to what the card can hold
   *before* each forward pass, and a batch the accelerator cannot
   allocate is retried at half the size (down to one row). The scratch
   buffer a pass needs is `rows x sequence^2 x heads x 4` bytes --
   quadratic in the sequence length -- so at 20 rows and a 1280-token
   window it is 2.10 GB, and that is exactly the size ONNX's arena
   names when it fails. Without this, a busy card made the server
   return 500 forever: the batch controller only learned from
   successes, and the watcher redelivered the affected documents every
   30 seconds, indefinitely.
4. **Qdrant upsert** -- deterministic point IDs (xxhash + UUID v5) enable idempotent re-indexing; every write waits until Qdrant has applied it, so a failed write is an error rather than a silent loss; chunks past the document's new end are deleted after the upsert succeeds. Rich payload with full metadata for filtered search.

## API endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | 200 with server status, the server's own `pid`, its device, collection and versions; 503 if the embedder is unusable or Qdrant is unreachable. The `pid` is the server's own process id — the only party that knows it, whether systemd, launchd or a hand-typed command started it |
| `/readiness` | GET | `ready` (embedder healthy and Qdrant answering), `capacity` (embedder slots), `in_flight`, `reason`; 503 when not ready |
| `/status` | GET | Collection stats (points, documents, device); `documents_count_truncated` if the count hit 1,000,000 |
| `/distill` | POST | Index a document (non-streaming). `content` is required |
| `/distill/stream` | POST | Index with NDJSON streaming progress. `content` is required |
| `/search` | POST | Hybrid search (dense + lexical, RRF-fused) with optional filters. `limit` defaults to 10 and is clamped to 200; `query` is at most 65,536 bytes (larger is a 400); an unparseable `year` filter is a 400. Reference-list chunks are demoted below the passages that discuss the topic (they are not dropped). The returned `score` is the fused RRF score — comparable between hits of one search, meaningless across searches |
| `/exists/{doc_id}`, `/docs` | GET | Per-document chunk count; distinct doc ids (`limit` up to 1,000,000, larger is a 400; `truncated` flags a partial list) |
| `/docs/titles` | GET | The title stored on each document's points (a document with none maps to `null`) — the input to `hs distill backfill-titles` |
| `/docs/markdown-paths` | GET | The storage key of each document's markdown, as carried on its points. The doc_id is *not* that key: a book ingested from Anna's Archive has a title-based doc_id while its markdown is keyed by a shard of the title |
| `/doc/{doc_id}/title` | POST | Write a title onto a document's points. A **payload** update: the vector is untouched, so it is not a re-embed and needs no embedding model. An empty title is a 400 |
| `/doc/{doc_id}`, `/collection/reset`, `/scrub-interstitials` | DELETE / POST | Destructive maintenance |
| `/collection/hnsw?collection=<name>` | POST | Enable HNSW on an existing collection with `hnsw.m`/`ef_construct`, capped at `hnsw.max_indexing_threads` (default 4). Returns immediately (Qdrant builds the graph in the background); a no-op that says so when already enabled. Never run at startup — use `hs distill hnsw enable --collection <name>` in a maintenance window |
| `/embedder/release` | POST | Drop the model of every idle pool slot now (`{"released": n}`); the next embed reloads it. Used by the host GPU broker before it hands the card to another tenant |

**Authentication.** `hs-distill-server` refuses to start unless `HS_BACKEND_TOKEN` (≥ 32 visible ASCII bytes, e.g. `openssl rand -hex 32`; the same value as the gateway and every client host) is set — put it in `~/.home-still/secrets.env`. Every route except `GET /health` and `GET /readiness` requires `Authorization: Bearer <token>` and answers 401 (JSON body) otherwise. `DistillClient` sends it automatically from the same variable.

Client errors (bad input, unknown collection) are HTTP 400; a missing dependency is 503; anything else is 500.

## Building from source

```bash
# --release builds need the tag the binary ships as (build-support/version.rs)
export HS_RELEASE_TAG=v0.0.1-rc.NNN

# Client only (lightweight, no ONNX deps)
cargo build --release -p hs-distill

# Server (needs the ONNX runtime and CUDA; the binary does not compile without `cuda`)
cargo build --release -p hs-distill --features server,cuda --bin hs-distill-server
```
