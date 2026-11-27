# home-still

home-still is a personal research engine: it finds open academic papers,
converts them to clean markdown, indexes them by meaning, and lets you and
your AI assistant search and read them, on hardware you own.

## Why

Most peer-reviewed knowledge is in papers that are free to read but hard to
use: keyword search misses ideas phrased differently, PDFs are hard for
software to read, and the good commercial tools charge per seat and keep
your library in their account. home-still gives one person, at home, their
own searchable library of the open literature, and answers grounded in
papers they can check. [Read more](docs/explanation/overview.md).

## What you can do

- [Find and download papers](docs/how-to/find-and-download-papers.md) from
  several academic providers at once, deduplicated and ranked.
- [Convert PDFs, HTML and EPUB to markdown](docs/how-to/convert-documents.md)
  with a layout model and a vision-language model, keeping pages, tables and
  formulas.
- [Search your library](docs/how-to/index-and-search.md) by meaning and by
  keyword together.
- [Open the source at the cited page and passage](docs/how-to/open-documents-with-links.md),
  on your network or away from home.
- [Keep personal documents](docs/how-to/manage-personal-documents.md), such
  as medical and financial records, in a private searchable store.
- [Use a local OpenAlex catalog](docs/how-to/use-the-openalex-catalog.md) for
  offline metadata and the citation graph.
- [Use the library from Claude and other MCP agents](docs/how-to/connect-claude-and-agents.md).
- [Run across your home machines](docs/tutorials/home-cloud-cluster.md) and
  [reach it remotely](docs/how-to/access-remotely.md) through one
  authenticated gateway.
- [Watch the whole pipeline](docs/reference/status-dashboard.md) on a live
  dashboard.
- [Upgrade every binary and service](docs/how-to/run-and-upgrade-services.md)
  with one command.

## Status

home-still is under active development and ships as release candidates
tagged `v0.0.1-rc.N` on
[GitHub Releases](https://github.com/home-still/home/releases); the
[changelog](CHANGELOG.md) lists what needs action when you upgrade.

- **Platforms:** the `hs` command runs on macOS (Intel and Apple Silicon),
  Linux (x86-64 and ARM64) and Windows.
- **Search** needs an NVIDIA GPU with CUDA for the distill server (Linux,
  x86-64), and a [myelin](https://github.com/home-still/myelin) server
  (which keeps its own Qdrant) with Ollama holding `bge-m3` on the GPU.
- **Conversion** needs a vision-language model backend: Ollama (Metal on
  Apple Silicon, CUDA on NVIDIA) or an OpenAI-compatible server.
- **Automation** needs a NATS server with JetStream.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
```

This installs `hs` and, where your platform has them, its companion servers
into `~/.local/bin/`.

## Quick start

These work right after installing, with nothing else running:

```sh
hs config init
hs paper search "transformer attention mechanisms" -n 5
hs paper search --type author "Hinton" --sort citations -n 5
hs paper get --doi 10.48550/arXiv.1706.03762
```

Downloading needs the event bus, conversion needs a model and its layout
files, and search needs the distill server and a myelin server.
[Build your first research library](docs/tutorials/first-library.md) sets up
each, step by step, on one machine, and ends with a library that converts
and indexes every paper you download by itself.

## Documentation

The [documentation index](docs/README.md) has four parts:

- [Tutorials](docs/README.md#tutorials): learn by building a working
  library.
- [How-to guides](docs/README.md#how-to-guides): get a specific task done.
- [Reference](docs/README.md#reference): every command, tool, setting, port
  and dashboard row.
- [Explanation](docs/README.md#explanation): how and why it works the way it
  does.

## Contributing

Building, testing, releasing and the documentation rules are in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Related projects

- [apple_health_source](https://github.com/home-still/apple_health_source):
  sync Apple Health to a database you own and query it from Claude Desktop
  over MCP.

## License

MIT
