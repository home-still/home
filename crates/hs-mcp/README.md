# hs-mcp

The [Model Context Protocol](https://modelcontextprotocol.io) server for
home-still. It offers the library to AI assistants as tools and resources:
search papers and the local corpus, read the catalog and the markdown,
convert and index documents, mint document links, and report the pipeline's
status. It speaks MCP over stdio (the default, for a client on the same
machine) or over streamable HTTP with `--serve <addr>`, which requires
`HS_BACKEND_TOKEN`.

## User documentation

- [Connect Claude and other agents](../../docs/how-to/connect-claude-and-agents.md)
- [MCP tools](../../docs/reference/mcp-tools.md): every tool, resource,
  parameter and the configuration the server reads.
- [`hs mcp` and `hs serve mcp` commands](../../docs/reference/cli.md#hs-mcp)

## Code map

```text
main.rs        the HomeStillMcp server: parameter structs, every #[tool] handler,
               resources, tool visibility, the status fan-out, and the docs_tests module
http_app.rs    the streamable HTTP transport, backend-token check, session idle timeout
links.rs       signed document links attached to tool results
stem.rs        stem validation for tool arguments
tool_tests.rs  tool behaviour tests; testkit.rs holds their stand-in services
```

`docs_tests` in `main.rs` fails when a tool is added, renamed or removed
without a matching `### \`<tool>\`` heading in
[`docs/reference/mcp-tools.md`](../../docs/reference/mcp-tools.md).

## Build and test

```sh
cargo test -p hs-mcp
cargo build -p hs-mcp
```
