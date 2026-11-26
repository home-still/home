# Paper providers

`hs paper search`, `hs paper download` and the MCP `discover` tool query
these providers. The keys are under `paper.providers.<name>` in
[configuration](configuration.md#paper).

## Providers

| Provider | `--provider` value | Coverage | Default request interval | Key |
|---|---|---|---|---|
| [arXiv](https://arxiv.org) | `arxiv` | Preprints in physics, mathematics, computer science and biology | 3000 ms (one request every 3 s) | none |
| [OpenAlex](https://openalex.org) | `openalex` | Works across all disciplines | 100 ms | optional `api_key` |
| [Semantic Scholar](https://www.semanticscholar.org) | `semanticscholar` (aliases `semantic_scholar`, `s2`) | Papers with citation graphs | 1100 ms | optional `api_key`; the anonymous tier is rate limited across all users |
| [Europe PMC](https://europepmc.org) | `europepmc` (aliases `europe_pmc`, `europmc`, `pmc`) | Biomedical and life sciences | 200 ms | none |
| [CrossRef](https://www.crossref.org) | `crossref` | DOI registration records | 100 ms | optional `mailto` |
| [CORE](https://core.ac.uk) | `core` | Open-access repository papers | 2100 ms | `api_key` required |

The interval is `rate_limit_interval_ms`, the least time between two requests
to one provider. Each provider also has `base_url` and `timeout_secs` (default
30); Semantic Scholar has `max_retry_after_secs` (default 30), the longest a
`Retry-After` may make a request wait.

`--provider all` (the default) queries every provider in parallel. CORE is in
`all` only when `paper.providers.core.api_key` is set; without a key it is
left out (logged once: `CORE provider disabled: no
paper.providers.core.api_key`), and `--provider core` reports the missing key.

## Search

| Flag | Values | Default |
|---|---|---|
| `-t`, `--type` | `keywords`, `title`, `author`, `doi`, `subject` | `keywords` |
| `-p`, `--provider` | `all` or a provider value above | `all` |
| `-s`, `--sort` | `relevance`, `date`, `citations` | `relevance` |
| `-n`, `--max-results` | number | 10 |
| `-d`, `--date` | `>=2024`, `>2023 <2025`, `>=2024-06` | none |
| `-a`, `--abstract` | show abstracts | off |
| `--min-citations` | minimum citation count | none |
| `--offset` | result offset | 0 |

### Merging and ranking

Results from all providers are deduplicated by DOI and by fuzzy title match,
then ranked by a blend of reciprocal rank fusion across providers, how well
the title and content match the query, and a log-scaled citation count. There
is no recency term.

| `--sort` | Order |
|---|---|
| `relevance` | The blended score. |
| `date` | Newest first; papers with no date last; ties in blended order. |
| `citations` | Most cited first; papers with no count last; ties in blended order. Also drops papers whose title or content does not match the query. |

### Query syntax

- Plain multi-word text is searched as a phrase.
- On arXiv, `AND`, `OR` and `NOT` between terms are operators, and
  `"quoted phrases"` stay together. Two terms with no operator between them
  mean `AND`. An unclosed quote is an error.
- A DOI (for `hs paper get`, `--doi`, and the `paper_get` tool) may be bare,
  prefixed `doi:`, or a `https://doi.org/` URL, in any case.

### Paging

| Provider | `--offset` |
|---|---|
| OpenAlex | Must be a multiple of `-n` (it pages by page number). |
| Europe PMC | Not supported (it pages by cursor): an error when searched alone, and Europe PMC is left out of `all` when the offset is not 0. |
| Others | Any offset. |

`--provider all` never reports a next offset.

## Download

`hs paper download` finds a PDF for each paper by trying these sources in
order and stops at the first that yields one:

1. arXiv, for arXiv DOIs (`10.48550/arXiv.…`).
2. MDPI, for DOIs starting `10.3390/` (all MDPI journals are open access).
3. Unpaywall, when `paper.download.unpaywall_email` is set.
4. PubMed Central.
5. The providers' own links: Semantic Scholar, Europe PMC, CORE (with a key),
   OpenAlex, CrossRef.

| Rule | Detail |
|---|---|
| PDF only | The body must start with `%PDF-`. Landing pages, images and stubs are rejected and nothing is stored. |
| Size cap | The transfer stops at `paper.download.max_download_bytes` (default 256 MiB). |
| Public hosts only | Only `http`/`https` URLs without credentials. Loopback, private, link-local and metadata addresses are refused before the request, on every redirect and on every resolved address. When `HTTP(S)_PROXY` is set, the proxy resolves the host, so the address check does not apply. |
| Storage key | The stem is the lowercased bare DOI with `/` replaced by `_` (`10.1016/j.x` → `10.1016_j.x`), else the provider's id. A paper found by search and the same paper fetched by DOI share one key. |
| Failure report | A storage failure stops the download at once. Otherwise the error lists every source's outcome (`no copy` or `failed`). |
| Already stored | A paper whose key already exists in storage is not downloaded again. |
| Event | Each stored PDF publishes `papers.ingested`, so `hs paper download` needs an `events:` section. |

A query download over-requests the search by half, because papers without a
DOI or download URL are skipped before they count toward `-n`.

## Citation graph

The MCP tool [`graph`](mcp-tools.md#graph), with `source: "semantic_scholar"`,
uses the Semantic Scholar Graph API under the same rate limiter as search.

| `graph` parameter, for `direction: "citations"` | Meaning |
|---|---|
| `limit` | 1–1000, default 100. Counts papers that pass `year_from`. |
| `year_from` | Only citing papers from this year on; applied while pages are fetched. |
| `sort` | `year` (default) orders the first `limit` papers returned; `citations` ranks the whole citing set Semantic Scholar serves (about 9000 edges at most). Anything else is an error. |

`truncated: true` in the response means more citing papers exist than were
returned.
