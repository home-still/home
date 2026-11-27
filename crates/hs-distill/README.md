# hs-distill

Chunking, document stores and search. The library is the distill client used
by `hs` and `hs-mcp`; the `server` feature builds `hs-distill-server`, which
cuts markdown into passages, files them in named stores of a myelin server
(which embeds them with BGE-M3 through Ollama, on a CUDA GPU), and answers
searches over those stores.

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
metadata.rs               record metadata from the catalog entry sent with a request
reference_list.rs         reference-list detection: those chunks go to the references store
store.rs                  the DocumentStore seam the server uses (tests use an in-memory fake)
myelin.rs                 DocumentStore over a myelin MCP server: stores, records, search
embed_device.rs           where Ollama runs the embedding model; the server refuses work off the GPU
pipeline.rs               chunk → file by store → write for one document
abstracts.rs              abstract selection and extraction for the abstracts store
reconcile.rs              markdown / store / catalog reconciliation
event_watch.rs            the scribe.completed consumer behind hs distill watch-events
api.rs                    transport-independent request handling: validation, stores, limits
server.rs                 the axum adapter (feature server)
server_main.rs            hs-distill-server
```

## Build and test

```sh
cargo nextest run -p hs-distill                   # client
cargo nextest run -p hs-distill --features server # server modules
cargo clippy -p hs-distill --features server --all-targets -- -D warnings
```

The server binary needs the `server` feature:

```sh
cargo build -p hs-distill --features server --bin hs-distill-server
```
