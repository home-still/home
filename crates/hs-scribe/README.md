# hs-scribe

Conversion of PDF, HTML and EPUB documents to markdown. The library is the
scribe client and event watcher used by `hs` and `hs-mcp`; the `server`
feature builds `hs-scribe-server`, which renders each page with pdfium, finds
its regions with a layout model, and has a vision-language model read each
region.

## User documentation

- [Convert documents](../../docs/how-to/convert-documents.md)
- [How conversion works](../../docs/explanation/conversion.md)
- [`scribe` and `scribe_server` configuration](../../docs/reference/configuration.md#scribe_server)
- [Scribe server endpoints](../../docs/reference/services-and-ports.md#scribe-server-endpoints)
- [Conversion failure reasons](../../docs/reference/status-dashboard.md#conversion-failure-reasons)

## Code map

```text
client.rs               ScribeClient: upload, NDJSON progress stream, health and readiness
event_watch.rs          the papers.ingested consumer: convert, store markdown and catalog, publish
classify.rs             FailureCode wire tokens and their retryable / bad-source kind
config.rs               scribe / scribe_server sections and HS_SCRIBE_* overrides
pdfium.rs, pdf_meta.rs  binding libpdfium; page counts before dispatch
html.rs, epub.rs        the bounded HTML and EPUB converters
server.rs               the axum server, admission slots and the VRAM gate (feature server)
backend_probe.rs        whether the VLM backend can start the model now
pipeline/               PDF parsing, the per-page processor, markdown assembly
models/                 PP-DocLayout-V3 layout and SLANet-Plus table models (ONNX)
ocr/                    VLM backends (Ollama, OpenAI-compatible, cloud) and the repetition detector
converter/              the olmocr subprocess converter
postprocess.rs          repetition cleanup on finished pages
diag.rs                 opt-in per-page diagnostics (HS_SCRIBE_DIAG_DIR)
eval/                   the OCR benchmark harness (feature eval)
python/                 legacy scripts, unused by the current pipeline
```

## Build and test

```sh
cargo test -p hs-scribe                              # client
cargo test -p hs-scribe --features server            # server
cargo test -p hs-scribe --features cuda --lib        # the Linux release feature set
cargo test -p hs-scribe --features eval --lib eval   # benchmark harness
```

Server tests that need libpdfium print `SKIPPED` without it; set
`LD_LIBRARY_PATH` to a directory holding `libpdfium.so` to run them.

`--features cuda` (which implies `server`) registers the CUDA execution
provider for the layout and table models and is what the Linux release
ships. The macOS release is built with `--features server`.

The container image is built from `docker/Dockerfile`.
