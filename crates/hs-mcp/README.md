# hs-mcp

[Model Context Protocol](https://modelcontextprotocol.io) server for the home-still research pipeline. Exposes the full read API as MCP tools for use with Claude Desktop, Claude Code, and other MCP clients.

## Tools

| Tool | Parameters | Description |
|------|-----------|-------------|
| **paper_search** | query, max_results?, search_type?, date? | Search 6 academic providers |
| **paper_get** | doi | Look up a paper by DOI |
| **catalog_list** | - | List all papers with titles and conversion status |
| **catalog_read** | stem | Full catalog metadata (authors, DOI, conversion info) |
| **markdown_list** | - | List converted documents with sizes and page counts |
| **markdown_read** | stem, page? | Read full document or a single page |
| **scribe_health** | - | Scribe server health, readiness, version |
| **scribe_convert** | pdf_path | Convert a PDF to markdown |
| **distill_search** | query, limit?, year?, topic? | Semantic search with filters |
| **distill_status** | - | Qdrant collection stats and server health |
| **distill_exists** | doc_id | Check if a document is indexed |
| **system_status** | - | Full pipeline stats (PDFs, markdown, embedded, services) |

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

The recommended way to run the MCP server remotely. Automatically registers with the gateway, manages lifecycle (heartbeat, deregistration on shutdown):

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
