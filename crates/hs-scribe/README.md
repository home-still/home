# pdf-mash

PDF-to-markdown converter using ONNX layout detection and vision-language model OCR. Part of [home-still](../../README.md).

## How it works

1. **Layout detection** — DocLayout-YOLO via ONNX Runtime classifies page regions into 10 types: title, plain text, figures, tables, formulas, headers, footers, captions, references, equations.
2. **VLM OCR** — Detected regions are sent to a vision-language model for text extraction and structured output.
3. **Markdown assembly** — Regions are ordered and assembled into section-aware markdown with heading hierarchy, figure/table captions, and formula blocks.

## VLM backends

| Backend | Flag/Config | Use case |
|---|---|---|
| Ollama | `--ollama-url` | Local inference |
| OpenAI-compatible | `--openai-url` | vLLM, sglang, MLX serving |
| Cloud | `--cloud-provider` | OpenAI, Anthropic |

## Model setup

Layout detection requires a DocLayout-YOLO ONNX model in `models/`:

```sh
cd python
uv sync
uv run python convert_to_onnx.py /path/to/doclayout-yolo.pt
mv model.onnx ../models/
```

See [python/README.md](python/README.md) for details.

## Build

```sh
cargo check -p pdf-mash
cargo build --release -p pdf-mash

# With evaluation harness
cargo build --release -p pdf-mash --features eval
```

## Features

- GPU acceleration via ONNX Runtime (CUDA + CPU fallback)
- Watch mode for development (`--watch`)
- Evaluation harness with BLEU, TED, and edit distance metrics (behind `eval` feature flag)
- Configurable image dimensions and VLM concurrency
