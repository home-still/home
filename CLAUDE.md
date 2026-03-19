# home-still

Academic research engine: 211M+ vector search with OpenAlex + PMC OA + Qdrant.

## Build & Test

```bash
cargo check                        # Check all crates
cargo test                         # Run all tests
cargo test -p paper                # Test paper crate
cargo build --release -p paper     # Build paper CLI
cargo check -p pdf-mash            # Check pdf-mash
```

## Architecture

Workspace crates:
- `paper` — Academic paper meta-search CLI: arXiv, OpenAlex, aggregation, download (git submodule)
- `pdf-mash` — PDF-to-markdown pipeline with ONNX layout detection + VLM OCR (git submodule, in `pdf_masher/pdf-mash`)
- `hs-style` — Shared CLI styling (git submodule)
- `crates/*` — Future workspace-local crates (distill pipeline, etc.)

## Data layout

- `data/openalex` -> OpenAlex snapshot (311 partitions, plain JSONL)
- `data/qdrant` -> Qdrant storage on dedicated NVMe
- `data/downloads` -> PMC/CORE downloads
- `data/processed/` -> Intermediate processed data

## Pipeline flow

```
openalex-ingest -> chunker -> embedder -> qdrant-sink
```

Connected via bounded crossbeam channels with backpressure.

## Conventions

- Use `thiserror` for library error types, `anyhow` in binaries
- Use `tracing` for logging (not `log`)
- Config via figment (YAML + env vars)
- Deterministic point IDs: UUID v5 from xxhash(doc_id + chunk_index)
