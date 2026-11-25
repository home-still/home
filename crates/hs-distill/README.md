# hs-distill

Chunking, embedding and search. The library is the distill client used by
`hs` and `hs-mcp`; the `server` feature builds `hs-distill-server`, which
embeds passages with BGE-M3 on a CUDA GPU, stores them in Qdrant, and answers
hybrid (dense + lexical) searches.

## User documentation

- [Index and search your library](../../docs/how-to/index-and-search.md)
- [How search works](../../docs/explanation/search.md)
- [`distill` and `distill_server` configuration](../../docs/reference/configuration.md#distill_server)
- [Distill server endpoints](../../docs/reference/services-and-ports.md#distill-server-endpoints)
- [`hs distill` commands](../../docs/reference/cli.md#hs-distill)

## Code map

```text
client.rs, cli.rs         DistillClient and the hs distill argument types (feature client)
config.rs                 distill / distill_server sections, HS_DISTILL_* overrides, validation
chunker.rs, text.rs       sentence- and page-aware chunking
quality.rs                the chunk-quality floor
metadata.rs               payload metadata from the catalog entry sent with a request
embed/                    the BGE-M3 ONNX embedder, its slot pool, VRAM floor and idle release
adaptive_batch.rs         batch sizing against throughput and the card's free memory
store.rs                  the VectorStore seam the server uses (tests use an in-memory fake)
qdrant.rs, collection.rs  collections, indexes, upserts, the dense and lexical channels
pipeline.rs               chunk → embed → upsert for one document
fusion.rs                 reciprocal rank fusion
reference_list.rs         reference-list detection for demotion
abstracts.rs              the paper_abstracts collection and abstract extraction
reconcile.rs, backfill.rs markdown / Qdrant / catalog reconciliation and title backfill
event_watch.rs            the scribe.completed consumer behind hs distill watch-events
api.rs                    transport-independent request handling: validation, routing, limits
server.rs                 the axum adapter (feature server)
server_main.rs            hs-distill-server; refuses to compile without the cuda feature
```

## Build and test

```sh
cargo test -p hs-distill                         # client
cargo test -p hs-distill --features server --lib # server modules
cargo clippy -p hs-distill --features cuda --all-targets -- -D warnings
```

The server binary needs the `cuda` feature (which implies `server`) and
downloads the ONNX Runtime CUDA bundle on first build:

```sh
cargo build -p hs-distill --features cuda --bin hs-distill-server
```
