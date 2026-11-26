# Index and search your library

Run the distill server, index converted markdown, search it, and keep the
index consistent with your library. How search ranks passages is explained in
[search](../explanation/search.md).

## Before you start

- The distill server host has an NVIDIA GPU with CUDA, a myelin server and
  Ollama running, `hs-distill-server` in `~/.local/bin/` (the install script
  puts it there on x86-64 Linux), and `HS_BACKEND_TOKEN` in
  `~/.home-still/secrets.env`.
- Every host that indexes or searches lists the server in `distill.servers`
  and has the same `HS_BACKEND_TOKEN`. See
  [`distill`](../reference/configuration.md#distill).
- The server's own settings (the myelin address, chunking, metadata) and the
  stores it files documents in are in
  [`distill_server`](../reference/configuration.md#distill_server).

## Start the server

1. Start the server in the background:

   ```sh
   hs serve distill start
   ```

   It defines its stores in the myelin server at `distill_server.myelin_url`
   and stops if that server does not answer or refuses one.

2. To run it as a service that starts at boot:

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

A search covers the `academic` store unless you name others with `--store`,
which you can repeat:

```sh
hs distill search "hippocampal neurogenesis" --store references
hs distill search "tax return 2024" --store personal --store academic
```

## Search papers by their abstracts

Build the per-paper `abstracts` store once, then keep it current:

```sh
hs distill abstracts build
hs distill abstracts reconcile
hs distill abstracts status
```

`build --force` re-embeds papers that are already in it. An agent searches it
with `recall` on the `myelin` server, naming the store: `stores: ["abstracts"]`.

## Keep the index consistent

1. Report documents whose markdown never reached the store, or whose catalog
   entry lost its embedding stamp:

   ```sh
   hs distill reconcile
   ```

2. Repair them:

   ```sh
   hs distill reconcile --reembed --fix-stamps
   ```

3. Find out why a document produces no passages:

   ```sh
   hs distill diagnose 10.48550_arxiv.1706.03762
   ```

4. Delete every passage of one document from the `academic` store, for
   example after its markdown was removed:

   ```sh
   hs distill purge 10.48550_arxiv.1706.03762
   ```

## Check it worked

```sh
hs distill status
```

It shows the server, its GPU, and the `academic` store's chunk count.
On the dashboard, the Embedded row approaches the Markdown row.
