# home-still documentation

The documentation is in four parts. Learn by doing in a **tutorial**, get a
task done with a **how-to guide**, look a fact up in the **reference**, or
understand the design in an **explanation**.

## Tutorials

| Page | What you build |
|---|---|
| [Build your first research library](tutorials/first-library.md) | One Linux machine that downloads, converts, indexes and searches papers by itself, connected to Claude. |
| [Run home-still across your home machines](tutorials/home-cloud-cluster.md) | A small cluster: shared storage, a GPU node, Qdrant, NATS, and a gateway that makes the library reachable from anywhere. |

## How-to guides

| Page | Task |
|---|---|
| [Find and download papers](how-to/find-and-download-papers.md) | Search the academic providers, look papers up by DOI, download them, and add files you downloaded yourself. |
| [Convert documents](how-to/convert-documents.md) | Run scribe servers, convert files by hand or automatically, and retry failed or stalled conversions. |
| [Index and search your library](how-to/index-and-search.md) | Run the distill server, index markdown, search, build the abstracts collection, and repair the index. |
| [Open documents at the cited page](how-to/open-documents-with-links.md) | Serve signed links that open a source at the page, or the markdown at the passage, on the LAN and remotely. |
| [Manage personal documents](how-to/manage-personal-documents.md) | Keep private records (medical, financial and others) in their own searchable store. |
| [Use the local OpenAlex catalog](how-to/use-the-openalex-catalog.md) | Load an OpenAlex snapshot for offline metadata, full-text search and the citation graph. |
| [Connect Claude and other agents](how-to/connect-claude-and-agents.md) | Give Claude Desktop, Claude Code or OpenCode the home-still tools, locally or through the gateway. |
| [Access your library remotely](how-to/access-remotely.md) | Enroll and revoke devices, get tokens, and rotate the gateway's signing secret. |
| [Run and upgrade services](how-to/run-and-upgrade-services.md) | Start, install and restart the services, upgrade every binary, and watch the dashboard. |
| [Operate a cluster](how-to/operate-a-cluster.md) | Verify a cluster, roll out upgrades, add or retire nodes, choose and migrate storage, and recover Qdrant. |
| [Troubleshoot](how-to/troubleshoot.md) | Match a symptom to its cause and fix. |

## Reference

| Page | Contents |
|---|---|
| [`hs` command reference](reference/cli.md) | Every command, flag, exit code and example. |
| [MCP tools](reference/mcp-tools.md) | Every tool and resource the MCP server offers, with parameters. |
| [Configuration](reference/configuration.md) | Every configuration key, environment variable and secret. |
| [Services, ports and endpoints](reference/services-and-ports.md) | Ports, service units, HTTP endpoints and event subjects. |
| [`hs status` dashboard](reference/status-dashboard.md) | Every dashboard row and every conversion failure reason. |
| [Paper providers](reference/paper-providers.md) | The search providers, search syntax, ranking and the download chain. |
| [Glossary](reference/glossary.md) | The terms these pages use. |

## Explanation

| Page | Question it answers |
|---|---|
| [What home-still is and why it exists](explanation/overview.md) | What problem it solves, for whom, and how the four stages fit together. |
| [Architecture](explanation/architecture.md) | How storage, events, servers, the gateway and the crates fit together, and why there is one path for everything. |
| [How conversion works](explanation/conversion.md) | How PDFs become markdown, what the quality gates reject, and why failures are retryable or not. |
| [How search works](explanation/search.md) | How passages are indexed and ranked, and how a hit links to its page. |

Changes that need action when you upgrade are in the
[changelog](../CHANGELOG.md). To work on home-still itself, see
[contributing](../CONTRIBUTING.md).
