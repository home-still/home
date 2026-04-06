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

### Streamable HTTP / SSE (remote)

For remote access via the cloud gateway:

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

The MCP server reads the same `~/.home-still/config.yaml` as the CLI. It uses:

- `scribe.servers` — to connect to scribe backends
- `distill.servers` — to connect to distill backends
- `scribe.output_dir` / `scribe.watch_dir` / `scribe.catalog_dir` — for filesystem tools
- `home.project_dir` — base directory for papers and markdown

## Build

```sh
cargo build --release -p hs-mcp
```
