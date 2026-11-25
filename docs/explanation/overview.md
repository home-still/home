# What home-still is and why it exists

home-still is a personal research engine. It gathers open academic papers,
turns them into clean text, indexes that text by meaning, and lets you, or an
AI assistant working for you, search and read the library. It runs on your
own machines and needs no subscription.

## Why

Most peer-reviewed knowledge sits in academic papers, and tens of millions of
them are free to read. Few people outside a university or a well-funded
company can use them well, for four reasons:

- **Search matches words, not ideas.** If you do not know a field's jargon,
  keyword search does not find what you mean.
- **PDFs are hard for software to read.** A paper is a picture of text in
  columns, with figures, tables and equations, and most tools give up on it.
- **Reading takes hours.** You read a paper carefully before you know whether
  it answers your question.
- **Good commercial tools charge per seat** and keep the library in their
  account.

home-still addresses each one. It reads PDFs with a layout model and a
vision-language model, indexes the text by meaning and by keyword, and gives
an AI assistant the tools to search and read your library, so you can ask a
question in plain language and get an answer grounded in papers you can
check. The library is files on your disk; a publisher changing its mind does
not take it away.

## Who it is for

- Independent researchers and writers who need to understand a literature,
  not just locate it.
- Students who want a library that stays theirs after they graduate.
- Hobbyists and citizen scientists who go deep on niche topics.
- Journalists and policy analysts who ground claims in primary sources.

It is built for people who would otherwise have nothing, not for
institutions that can buy a commercial service.

## The four stages

```text
   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
   │  1 Acquire  │──▶ │  2 Convert  │──▶ │  3 Index    │──▶ │  4 Search   │
   │  hs paper   │    │  hs scribe  │    │  hs distill │    │  hs distill │
   │             │    │             │    │             │    │   search    │
   │  open       │    │  PDF, HTML, │    │  chunks →   │    │  by meaning │
   │  academic   │    │  EPUB →     │    │  vectors +  │    │  and by     │
   │  databases  │    │  markdown   │    │  text index │    │  keyword    │
   └─────────────┘    └─────────────┘    └─────────────┘    └─────────────┘
        │                  │                  │                  │
        ▼                  ▼                  ▼                  ▼
   sources in         markdown in        points in          passages with
   papers/            markdown/          Qdrant             links to the
                                                            page they are on
```

1. **Acquire.** `hs paper search` asks several open academic databases at
   once and merges the answers; `hs paper download` stores the PDFs. You can
   also drop files you downloaded yourself into an inbox folder. See
   [paper providers](../reference/paper-providers.md).
2. **Convert.** The scribe server finds the regions of each page (text,
   tables, formulas, figures) and has a vision-language model read each one.
   The result is markdown with the source's pages kept apart. See
   [how conversion works](conversion.md).
3. **Index.** The distill server cuts the markdown into passages, turns each
   into a vector that captures its meaning, and stores it in Qdrant together
   with its text and metadata.
4. **Search.** A query runs two ways at once, by meaning and by keyword, and
   the two rankings are fused. Each hit names its paper and page, and can
   carry a link that opens the source at that page. See
   [how search works](search.md).

Each stored document publishes an event, and watchers pick the events up: a
downloaded PDF is converted, and converted markdown is indexed, without
anyone running a command. The [status dashboard](../reference/status-dashboard.md)
shows the whole pipeline.

## Using it through an AI assistant

Search returns passages. To get a synthesis across several papers, you
connect an MCP client such as Claude to home-still's
[MCP server](../reference/mcp-tools.md). The assistant can then search your
library, read whole papers, follow citations and hand you links to the exact
page, and answer with citations you can verify.

## What runs where

On one machine, everything runs side by side: the `hs` command, a scribe
server, a distill server, Qdrant, NATS and Ollama.
[Build your first research library](../tutorials/first-library.md) sets that
up.

A larger library spreads across a few home machines, each doing the smallest
job its hardware suits:

- **Storage** (a small computer with a large SSD) holds the papers, markdown
  and catalog, as a shared filesystem or an S3-compatible Garage store.
- **GPU compute** (a desktop with an NVIDIA card) runs conversion and
  embedding. It is the only machine that needs real horsepower, and the
  distill server requires CUDA.
- **Qdrant** runs on a host with fast disk.
- **The gateway host** runs Cloudflare's tunnel agent and `hs-gateway`, the
  one program reachable from the internet.
- **Your laptops** run the `hs` command and Claude.

The split exists because cheap hardware does not have everything in one box,
because only one machine should face the internet, because a failure should
stay local (the storage keeps your papers if the GPU box dies), and because a
second GPU node should be one more line of configuration.
[Run home-still across your home machines](../tutorials/home-cloud-cluster.md)
builds that setup, and [architecture](architecture.md) describes how the
parts talk to each other.
