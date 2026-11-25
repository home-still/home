# Index and search your library

Run the distill server, index converted markdown, search it, and keep the
index consistent with your library. How search ranks passages is explained in
[search](../explanation/search.md).

## Before you start

- The distill server host has an NVIDIA GPU with CUDA, Docker or Podman (for
  Qdrant), `hs-distill-server` in `~/.local/bin/` (the install script puts it
  there on x86-64 Linux), and `HS_BACKEND_TOKEN` in
  `~/.home-still/secrets.env`.
- Every host that indexes or searches lists the server in `distill.servers`
  and has the same `HS_BACKEND_TOKEN`. See
  [`distill`](../reference/configuration.md#distill).
- The server's own settings (Qdrant address, collections, embedding) are in
  [`distill_server`](../reference/configuration.md#distill_server).

## Start the server

1. Set up Qdrant on this host. This writes
   `~/.home-still/docker-compose-distill.yml` and starts the container:

   ```sh
   hs distill init
   ```

   If Qdrant already runs elsewhere, set `distill_server.qdrant_url` instead
   and skip this step.

2. Start the server in the background:

   ```sh
   hs serve distill start
   ```

   When the compose file from step 1 exists, this also starts Qdrant; without
   it, the server requires Qdrant to be reachable already.

3. To run it as a service that starts at boot:

   ```sh
   hs serve distill --install
   ```

## Index

1. Index every markdown file under the markdown directory:

   ```sh
   hs distill index
   ```

   Documents already indexed are skipped; `--force` indexes everything again.
   `--file` indexes only the named files:

   ```sh
   hs distill index --file ~/home-still/markdown/10/10.48550_arxiv.1706.03762.md
   ```

2. To index every conversion as it is stored, install the distill watcher:

   ```sh
   hs serve distill-watch --install
   ```

   Or run it in the foreground with `hs distill watch-events`.

## Search

```sh
hs distill search "what makes attention computationally expensive"
```

Filter by year or topic, and change the number of hits:

```sh
hs distill search "sleep and memory consolidation" --year ">2020" --limit 20
hs distill search "gut microbiome" --topic "Neuroscience"
```

For scripts, add `--output json`.

## Search papers by their abstracts

Build the per-paper abstracts collection once, then keep it current:

```sh
hs distill abstracts build
hs distill abstracts reconcile
hs distill abstracts status
```

`build --force` re-embeds papers that are already in it. The MCP tool
[`abstract_search`](../reference/mcp-tools.md#abstract_search) searches it.

## Keep the index consistent

1. Report documents whose markdown never reached Qdrant, or whose catalog
   entry lost its embedding stamp:

   ```sh
   hs distill reconcile
   ```

2. Repair them:

   ```sh
   hs distill reconcile --reembed --fix-stamps
   ```

3. Copy missing titles from the catalog onto indexed passages (reports first,
   writes with `--apply`):

   ```sh
   hs distill backfill-titles
   hs distill backfill-titles --apply
   ```

4. Find out why a document produces no passages:

   ```sh
   hs distill diagnose 10.48550_arxiv.1706.03762
   ```

5. Delete every passage of one document from Qdrant, for example after its
   markdown was removed:

   ```sh
   hs distill purge 10.48550_arxiv.1706.03762
   ```

## Speed up a large collection

Qdrant answers from a brute-force scan until an HNSW index is built. Start the
background build in a quiet period:

```sh
hs distill hnsw enable --collection academic_papers
hs distill hnsw enable --collection academic_papers --yes
```

Without `--yes` it prints what it would do and exits with status 1.

## Check it worked

```sh
hs distill status
```

It shows the server, its GPU, and the collection's point and document counts.
On the dashboard, the Embedded row approaches the Markdown row.
