# Build your first research library on one machine

In this tutorial you install home-still on one Linux computer, download a
paper, convert it to markdown, index it, and search it by meaning. At the end
the pipeline runs by itself: every paper you download is converted and
indexed without another command, and Claude can search your library.

It takes about an hour, most of it downloads.

## What you need

- Linux on x86-64 with an NVIDIA GPU (8 GB of memory or more) and a working
  NVIDIA driver with CUDA. The search server only runs on the GPU.
- ONNX Runtime's CUDA provider, `libonnxruntime_providers_cuda.so`, under
  `~/.cache/ort.pyke.io/dfbin/`. The search and scribe servers load it from
  there. The release archives do not include it; a build of home-still with
  `--features cuda` puts it there (see [contributing](../../CONTRIBUTING.md)).
- [Docker](https://docs.docker.com/get-docker/) or Podman, for Qdrant and
  NATS.
- [Ollama](https://ollama.com/download), which runs the model that reads
  your PDFs.
- `curl`, `tar`, `openssl` and `sha256sum`.

## 1. Install home-still

```sh
curl -fsSL https://raw.githubusercontent.com/home-still/home/main/docs/install.sh | sh
hs --version
```

The script installs `hs`, `hs-distill-server`, `hs-mcp` and `hs-gateway`
into `~/.local/bin/`, checking each archive's checksum, and `hs --version`
prints the release, for example `hs 0.0.1-rc.388`. If the shell cannot find
`hs`, add `~/.local/bin` to your `PATH` as the script tells you.

## 2. Create your configuration

```sh
hs config init
hs config path
```

`hs config init` asks for an email address for Unpaywall (it finds more open
copies of papers; recommended), a CORE API key and an S3 secret. Press Enter
to skip the last two. It writes `~/.home-still/config.yaml` and creates
`~/home-still/` with `papers/`, `markdown/` and `catalog/` inside.
`hs config path` prints `/home/<you>/.home-still/config.yaml`.

## 3. Start the event bus and set the shared secret

Every stored document announces itself on an event bus, and the conversion
and indexing workers listen there, so `hs paper download` needs the bus
before it can store anything. Start NATS with JetStream:

```sh
docker run -d --name nats --restart unless-stopped -p 4222:4222 nats -js
```

Open `~/.home-still/config.yaml` and add these two sections at the end:

```yaml
events:
  backend: nats
  nats:
    url: nats://localhost:4222

distill:
  servers:
    - http://localhost:7434
```

The servers you start below refuse every request that does not carry a
shared secret. Create it once:

```sh
echo "HS_BACKEND_TOKEN=$(openssl rand -hex 32)" >> ~/.home-still/secrets.env
chmod 600 ~/.home-still/secrets.env
```

Every home-still program reads `secrets.env` when it starts.

## 4. Find and download a paper

```sh
hs paper search "attention is all you need" -n 5
```

You see five papers, merged from several academic databases, each with its
title, authors, year, citation count and DOI. Download one by its DOI:

```sh
hs paper download --doi 10.48550/arXiv.1706.03762
```

The PDF lands in `~/home-still/papers/10/10.48550_arxiv.1706.03762.pdf`, and
an event announcing it waits on the bus for a converter.

## 5. Set up conversion

The scribe server needs four things: the model that reads text, two small
layout models, the word OCR's files, and the PDF library pdfium.

Pull the reading model into Ollama:

```sh
ollama pull glm-ocr
```

Put the layout model `pp-doclayoutv3.onnx` (PP-DocLayout-V3, from
[PaddleX](https://github.com/PaddlePaddle/PaddleX)) and the table model
`slanet-plus.onnx` (SLANet-Plus, from
[PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)) in
`~/.home-still/models/`, as ONNX files with exactly those names:

```sh
mkdir -p ~/.home-still/models
ls ~/.home-still/models
```

`ls` lists `pp-doclayoutv3.onnx` and `slanet-plus.onnx`.

Put the word OCR's three files beside them. The server uses them to find the
words on a scanned page, so that a quotation can be highlighted in a PDF that
has no text layer:

```sh
cd ~/.home-still/models
for f in pp-ocrv5_mobile_det.onnx en_pp-ocrv5_mobile_rec.onnx ppocrv5_en_dict.txt; do
  curl -fLO "https://github.com/GreatV/oar-ocr/releases/download/v0.3.0/$f"
done
```

Install pdfium and the scribe server from their releases:

```sh
cd "$(mktemp -d)"
curl -fLO "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7749/pdfium-linux-x64.tgz"
tar -xzf pdfium-linux-x64.tgz
mkdir -p ~/.local/lib && cp lib/libpdfium.so ~/.local/lib/

VERSION="v$(hs --version | cut -d' ' -f2)"
ARCHIVE="hs-scribe-server-$VERSION-x86_64-unknown-linux-gnu.tar.gz"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE"
curl -fLO "https://github.com/home-still/home/releases/download/$VERSION/$ARCHIVE.sha256"
sha256sum -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE" -C ~/.local/bin hs-scribe-server
```

`sha256sum -c` prints `OK`. Open a second terminal and start the scribe
server there; leave it running:

```sh
hs serve scribe
```

It loads the layout models and listens on port 7433. Back in the first
terminal, convert the paper:

```sh
hs scribe convert ~/home-still/papers/10/10.48550_arxiv.1706.03762.pdf --out attention.md
```

While it converts you see each stage: parsing pages, detecting the layout,
reading each region. `attention.md` then holds the paper as markdown, its
pages separated by `---`, with headings, tables and formulas.

## 6. Convert the paper into your library

`hs scribe convert` wrote a file where you asked. To put the conversion into
the library, run the converter that listens on the bus. In a third terminal:

```sh
hs scribe watch-events
```

It picks up the event your download left in step 4, sends the PDF to the
scribe server, and stores the markdown under `~/home-still/markdown/10/` with
a catalog entry under `~/home-still/catalog/`. It then has the server build
the PDF's word boxes, which record where each word sits on its page, and
stores them under `~/home-still/boxes/`. Leave it running.

## 7. Index and search

```sh
hs serve distill start
hs distill index
```

`hs serve distill start` starts the search server, which keeps passages in a
myelin server and needs that server and Ollama running first (see
[`distill_server`](../reference/configuration.md#distill_server));
`hs distill index` cuts your markdown into passages, embeds them and stores
them, showing a progress bar. Now ask a question:

```sh
hs distill search "why does self-attention replace recurrence"
```

You see the best passages, each with its paper's title, year, page and a
relevance score, found by meaning even where they share none of your words.

## 8. Ask Claude about your library

```sh
hs mcp install --client desktop
```

This adds home-still to Claude Desktop's configuration. Restart Claude
Desktop; in a new chat, ask "Search my home-still library for how the
Transformer's attention is computed, and cite the page." Claude searches with
`recall`, checks each quote with `cite`, and answers with citations.

## 9. Make it automatic

Stop the scribe server from step 5 and the converter from step 6 with
Ctrl+C in their terminals. Then install the scribe server as a system service
(it asks for your `sudo` password), and the converter and indexer as user
services that start with your session and restart on failure:

```sh
hs serve scribe --install
hs serve scribe-watch --install
hs serve distill-watch --install
```

Now every download flows through on its own. Try it:

```sh
hs paper download "retrieval augmented generation" -n 3
```

To watch it happen, run the MCP server as a service and open the dashboard:

```sh
hs serve mcp --install
HS_MCP_URL=http://localhost:7445/mcp hs status
```

You see Documents climb by three, then Markdown, then Embedded, while the
In-flight row shows conversions running. Press `q` to leave.

## Where to go next

- [Find and download papers](../how-to/find-and-download-papers.md)
- [Convert documents](../how-to/convert-documents.md)
- [Index and search](../how-to/index-and-search.md)
- [Open documents at the cited page](../how-to/open-documents-with-links.md)
- [Connect Claude and other agents](../how-to/connect-claude-and-agents.md)
- [Run home-still across your home machines](home-cloud-cluster.md)
- [Fix a problem](../how-to/troubleshoot.md)
