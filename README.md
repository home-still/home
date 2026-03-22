# home-still

Free and open source tools to democratize knowledge acquisition, distillation, and comprehension.

## Quick start

```sh
cargo install --path crates/hs
hs config init
hs paper search "transformer attention mechanisms"
```

## What it does

home-still is a three-phase academic research engine:

| Phase | Tool | What it does | Status |
|---|---|---|---|
| **Acquire** | [`paper`](paper/) | Search and download papers from arXiv, OpenAlex, and more | Working |
| **Distill** | [`pdf-mash`](pdf_masher/pdf-mash/) | Convert PDFs to markdown using ONNX layout detection + VLM OCR | Working |
| **Distill** | distill pipeline | Chunk, embed, and index 211M+ papers into Qdrant vector search | Planned |
| **Comprehend** | vector search | Semantic search across the full academic corpus | Planned |

```
paper search/download
       |
       v
   pdf-mash (PDF -> markdown)
       |
       v
   openalex-ingest -> chunker -> embedder -> qdrant-sink
       |                                          |
   311 JSONL partitions                   211M+ vectors (768-dim)
   (OpenAlex + PMC OA + CORE)            Qdrant on NVMe
```

## Paper search

Search arXiv and OpenAlex simultaneously. Results are deduplicated, merged, and ranked using reciprocal rank fusion.

```sh
# Keyword search
hs paper search "CRISPR gene editing"

# By author, limit to 5
hs paper search --type author "Hinton" -n 5

# Recent papers with abstracts
hs paper search "diffusion models" --date ">=2024" -a

# Sort by citations
hs paper search "attention is all you need" --sort citations

# Single provider
hs paper search "neural ode" -p arxiv
```

### Download

```sh
hs paper download "neural nets" -n 25
hs paper download --doi "10.48550/arXiv.2301.00001"
```

### JSON output

```sh
hs paper search "LLM reasoning" --output json | jq '.papers[].title'
```

### Search options

| Flag | Values | Default |
|---|---|---|
| `-t, --type` | keywords, title, author, doi, subject | keywords |
| `-p, --provider` | all, arxiv, openalex | all |
| `-s, --sort` | relevance, date, citations | relevance |
| `-n, --max-results` | 1-100 | 10 |
| `-d, --date` | `>=2024`, `>2023 <2025`, `>=2024-06` | none |
| `-a, --abstract` | show abstracts | off |
| `--offset` | pagination offset | 0 |

### Providers

| Provider | Coverage | Rate limit |
|---|---|---|
| [arXiv](https://arxiv.org) | Physics, math, CS, biology preprints | 1 req / 3s |
| [OpenAlex](https://openalex.org) | 250M+ works across all disciplines | 1 req / 100ms |

Planned: Semantic Scholar, CORE, CrossRef, PubMed.

## PDF-to-Markdown

[pdf-mash](pdf_masher/pdf-mash/) converts academic PDFs into structured markdown using a two-stage pipeline:

1. **Layout detection** — DocLayout-YOLO via ONNX Runtime (GPU-accelerated). Detects 10 region types: title, text, figures, tables, formulas, headers, footers, captions, references, equations.
2. **VLM OCR** — Sends detected regions to a vision-language model for text extraction. Supports Ollama (local), OpenAI-compatible APIs (vLLM, sglang, MLX), and cloud providers.

Output: section-aware markdown with proper heading hierarchy, figure/table captions, and formula blocks.

## Vector pipeline (planned)

Ingest 211M+ academic papers into a Qdrant vector database for semantic search.

| Stage | Crate | What it does |
|---|---|---|
| Ingest | `openalex-ingest` | Stream 311 JSONL partitions, reconstruct abstracts from inverted indices |
| Chunk | `chunker` | Split into 512-token chunks with 50-token overlap, contextual headers |
| Embed | `embedder` | nomic-embed-text v1.5 (768-dim) via ONNX Runtime + CUDA |
| Store | `qdrant-sink` | Bulk upsert with deterministic point IDs, scalar int8 quantization |

Data sources: OpenAlex abstracts (160M), PMC Open Access full text (51M), CORE filtered (150M).

Connected via bounded crossbeam channels with backpressure. Pipeline is fully spec'd — implementation walkthroughs are in [`slop/walkthroughs/`](slop/walkthroughs/).

## Configuration

```sh
hs config init          # creates ~/.home-still/config.yaml
hs config show          # prints resolved config
```

Config file: `~/.home-still/config.yaml`

```yaml
home:
  # log_level: info

paper:
  download_path: ~/Downloads/home-still/papers
  providers:
    arxiv:
      timeout_secs: 30
      rate_limit_interval_ms: 3000
    openalex:
      # api_key: your-key-here
      timeout_secs: 30
      rate_limit_interval_ms: 100
  download:
    max_concurrent: 4
    timeout_secs: 120
```

Override with environment variables: `HOME_STILL_PAPER_DOWNLOAD_PATH=/tmp/papers`

## Architecture

```
crates/hs/          Unified CLI binary (hs paper search, hs config init, ...)
paper/              Academic paper meta-search library (arXiv, OpenAlex, aggregation)
pdf_masher/pdf-mash/  PDF-to-markdown (ONNX layout detection + VLM OCR)
hs-style/           Shared CLI styling (Reporter trait, progress bars, colors)
crates/             Future pipeline crates (distill-core, chunker, embedder, ...)
```

## Build

```sh
cargo build --release -p hs        # unified CLI
cargo check -p paper               # paper library
cargo check -p pdf-mash            # pdf-mash
cargo test --workspace --exclude pdf-mash
```
