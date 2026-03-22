# home-still

Academic paper search and download from your terminal. Searches arXiv and OpenAlex simultaneously.

## Quick start

```sh
cargo install --path crates/hs
hs config init
hs paper search "transformer attention mechanisms"
```

## Commands

| Command | What it does |
|---|---|
| `hs paper search` | Search for papers across providers |
| `hs paper download` | Search and download PDFs |
| `hs paper get` | Look up a single paper by DOI |
| `hs config init` | Generate default config at `~/.home-still/config.yaml` |
| `hs config show` | Print resolved configuration |
| `hs config path` | Print config file location |

## Examples

### Search

```sh
# Basic keyword search
hs paper search "CRISPR gene editing"

# Search by author, limit to 5 results
hs paper search --type author "Hinton" -n 5

# Recent papers with abstracts
hs paper search "diffusion models" --date ">=2024" -a

# Sort by citation count
hs paper search "attention is all you need" --sort citations

# Search a specific provider
hs paper search "neural ode" -p arxiv
```

### Download

```sh
# Download top 25 results for a query
hs paper download "neural nets" -n 25

# Download a specific paper by DOI
hs paper download --doi "10.48550/arXiv.2301.00001"

# Download with more concurrency
hs paper download "protein folding" -n 50 -c 8
```

### Get a single paper

```sh
hs paper get --doi "10.48550/arXiv.2301.00001"
```

### JSON output

```sh
# Pipe search results to jq
hs paper search "LLM reasoning" --output json | jq '.papers[].title'
```

## Search options

| Flag | Values | Default |
|---|---|---|
| `-t, --type` | keywords, title, author, doi, subject | keywords |
| `-p, --provider` | all, arxiv, openalex | all |
| `-s, --sort` | relevance, date, citations | relevance |
| `-n, --max-results` | 1-100 | 10 |
| `-d, --date` | `>=2024`, `>2023 <2025`, `>=2024-06` | none |
| `-a, --abstract` | show abstracts | off |
| `--offset` | pagination offset | 0 |

## Configuration

```sh
hs config init          # creates ~/.home-still/config.yaml
hs config show          # prints resolved config
```

Config file location: `~/.home-still/config.yaml`

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

Override any setting with environment variables: `HOME_STILL_PAPER_DOWNLOAD_PATH=/tmp/papers`

## Providers

| Provider | Coverage | Rate limit |
|---|---|---|
| [arXiv](https://arxiv.org) | Physics, math, CS, biology preprints | 1 req / 3s |
| [OpenAlex](https://openalex.org) | 250M+ works across all disciplines | 1 req / 100ms |

Both are searched by default. Results are deduplicated, merged, and ranked using reciprocal rank fusion.

## Build

```sh
cargo build --release -p hs
```
