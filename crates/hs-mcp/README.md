# hs-mcp

[Model Context Protocol](https://modelcontextprotocol.io) server for the home-still research pipeline. Exposes the full read API as MCP tools for use with Claude Desktop, Claude Code, and other MCP clients.

## Tools

| Tool | Parameters | Description |
|------|-----------|-------------|
| **paper_search** | query, max_results?, search_type?, date?, offset?, provider?, min_citations?, sort? | Search 5 academic providers (6 with a CORE API key) |
| **paper_get** | doi | Look up a paper by DOI |
| **paper_references** | doi | Structured reference list of a paper (Semantic Scholar) |
| **paper_citations** | doi, limit?, year_from?, sort? | Papers that cite a DOI (default limit 100, max 1000) |
| **paper_download** | doi | Download a paper PDF by DOI into the papers directory |
| **catalog_list** | limit?, offset?, embedded? | Papers with titles, conversion and embedded status |
| **catalog_recent** | limit?, include_repaired? | Most recent download/convert/embed events |
| **catalog_read** | stem | Full catalog metadata (authors, DOI, conversion info) |
| **catalog_repair** | dry_run, limit? | Report catalog ↔ storage reconciliation |
| **dedupe_url_encoded** | dry_run | Report URL-encoded duplicate stems (dry-run only) |
| **catalog_backfill_title** | dry_run, limit? | Backfill empty catalog titles for rows with a DOI |
| **markdown_list** | limit?, offset?, embedded? | Converted documents with sizes and page counts |
| **markdown_read** | stem, page? | Read full document or a single page |
| **scribe_health** | - | Scribe server health, readiness, version |
| **scribe_convert** | stem | Convert a stored PDF/HTML/EPUB to markdown (same path as the scribe watcher) |
| **distill_search** | query, limit?, year?, include_text? | Semantic search with filters |
| **abstract_search** | query, limit?, year?, include_text? | Semantic search over paper abstracts |
| **distill_status** | - | Qdrant collection stats and server health |
| **distill_exists** | doc_id | Check if a document is indexed |
| **distill_index** | stem | Index a converted markdown document |
| **distill_reindex** | stem | Re-index a document in place with fresh catalog metadata |
| **distill_reconcile** | dry_run, limit? | Report Qdrant doc_ids whose markdown is missing (dry-run only) |
| **distill_scan_repetitions** | limit?, threshold? | Report documents with VLM repetition artifacts (read-only) |
| **distill_backfill** | dry_run, limit?, retry_skipped | Index converted-but-not-embedded documents |
| **personal_search** | query, limit?, category? | Semantic search over personal documents |
| **personal_list** | limit?, category? | List ingested personal documents |
| **personal_read** | stem | Read a personal document's markdown |
| **personal_add** | filename, category?, title?, force | Ingest a file staged in the personal inbox |
| **personal_reindex** | stem | Re-chunk and re-embed a personal document |
| **system_status** | include_repaired? | Full pipeline stats (PDFs, markdown, embedded, services) |
| **openalex_search** | query, max_results?, year_from?, year_to?, min_citations?, sort? | BM25 search of the local OpenAlex catalog* |
| **openalex_get** | id_or_doi | One work from the local OpenAlex catalog* |
| **openalex_references** | openalex_id, limit? | Works a given work cites* |
| **openalex_citations** | openalex_id, limit?, year_from?, sort? | Works that cite a given work* |
| **openalex_authors_by_topic** | topic_id, limit? | Top authors for an OpenAlex topic* |

`?` marks an optional parameter. *The five `openalex_*` tools are listed only when the local OpenAlex corpus is built (`hs openalex build-fts`); restart the server after building it.

## Transport

### stdio (local)

For Claude Desktop or Claude Code running on the same machine as the MCP server:

```json
{
  "mcpServers": {
    "home-still": {
      "command": "hs-mcp"
    }
  }
}
```

### `hs serve mcp` (managed)

Runs `hs-mcp --serve 0.0.0.0:<port>` (default port 7445) in the foreground (`--install` registers it as a service). It does not register with the gateway: the gateway reaches it through the `mcp` entry of `cloud.gateway.routes`.

```sh
hs serve mcp
```

### Streamable HTTP / SSE (remote)

For manual remote access via the cloud gateway. The HTTP transport **requires** a shared backend token: set `HS_BACKEND_TOKEN` (at least 32 bytes, e.g. `openssl rand -hex 32`) in the environment or in `~/.home-still/secrets.env`; `hs-mcp --serve` refuses to start without it, and every request (any path) must carry `Authorization: Bearer <token>` or gets a 401 with a JSON-RPC error body. The gateway and any other client of this port must send the same secret (`AuthedHttp::plain` in `hs-common` does when the variable is set). Stdio mode needs no token. Idle sessions are dropped after `--session-idle-timeout-secs` (default 3600); clients should `DELETE` their session when done. Tools that run longer than that without progress keep their session alive by sending progress notifications when the caller supplied a progress token.

Notes on tool behavior: `paper_search` always returns `{"papers": [...], "provider_failures": [...]}`; `scribe_convert` takes a stem and converts the stored PDF/HTML/EPUB through the same functions the scribe watcher uses (existing markdown is re-announced, not converted again); `distill_reindex` replaces vectors in place and never deletes first. Every stem argument is validated (no `/`, `\`, `..`, NUL, empty) and rejected as invalid params.

```sh
hs-mcp --serve 127.0.0.1:7445
```

The gateway proxies `/mcp/*` to this server. Claude Desktop connects via `https://cloud.example.com/mcp` using OAuth2 (see [hs-gateway README](../hs-gateway/README.md)).

### systemd service

```ini
[Unit]
Description=Home-Still MCP Server (SSE)
After=network.target

[Service]
Type=simple
User=your-user
ExecStart=/path/to/hs-mcp --serve 127.0.0.1:7445
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

## Configuration

The MCP server reads the same `~/.home-still/config.yaml` as the CLI. It takes scribe and distill server addresses from the config (there is no service registry). It uses:

- `scribe.servers` — scribe backends
- `distill.servers` — distill backends
- `scribe.output_dir` / `scribe.watch_dir` / `scribe.catalog_dir` — for filesystem tools
- `home.project_dir` — base directory for papers and markdown

## Build

```sh
# --release needs the tag the binary ships as (build-support/version.rs)
HS_RELEASE_TAG=v0.0.1-rc.NNN cargo build --release -p hs-mcp
```
