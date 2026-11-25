# Find and download papers

Search the open academic databases, look papers up by DOI, and store their
PDFs in your library.

## Before you start

- `hs` is installed and `hs config init` has run.
- To download, your configuration has an `events:` section and the broker is
  running: every stored paper is announced on the event bus. See
  [`events`](../reference/configuration.md#events).
- Optional: set `paper.download.unpaywall_email` so Unpaywall can find more
  open copies, and `paper.providers.core.api_key` to add CORE. See
  [paper providers](../reference/paper-providers.md).

## Search

1. Search every provider by keyword:

   ```sh
   hs paper search "CRISPR gene editing"
   ```

2. Narrow the search. Search by author, keep five results:

   ```sh
   hs paper search --type author "Hinton" -n 5
   ```

   Only recent papers, with their abstracts:

   ```sh
   hs paper search "diffusion models" --date ">=2024" -a
   ```

   Most cited first, at least 100 citations:

   ```sh
   hs paper search "attention is all you need" --sort citations --min-citations 100
   ```

   One provider only:

   ```sh
   hs paper search "neural ode" -p arxiv
   ```

3. Page through results with `--offset`:

   ```sh
   hs paper search "protein folding" -n 10 --offset 10
   ```

4. For scripts, ask for JSON:

   ```sh
   hs paper search "LLM reasoning" --output json | jq '.papers[].title'
   ```

## Look up one paper

```sh
hs paper get --doi 10.1038/s41586-024-07487-w
```

The DOI may also be written `doi:10.…` or as a `https://doi.org/…` URL.

## Download

1. Download one paper by DOI:

   ```sh
   hs paper download --doi 10.48550/arXiv.2301.00001
   ```

2. Download the top matches of a search (the same `--type`, `--provider` and
   `--date` flags as `hs paper search`):

   ```sh
   hs paper download "neural nets" -n 25
   ```

3. Download more at once:

   ```sh
   hs paper download "transformers" -n 100 -c 8
   ```

A positional argument is always a search query: to download a single DOI,
pass `--doi`.

## Add papers you downloaded yourself

Copy a PDF, HTML or EPUB file into `papers/manually_downloaded/` under your
storage root, then sweep the inbox:

```sh
cp ~/Downloads/some-paper.pdf ~/home-still/papers/manually_downloaded/
hs scribe inbox sweep
```

To have it swept automatically, see
[convert documents](convert-documents.md#process-files-you-add-by-hand).

## Check it worked

Each stored PDF is under `papers/<first two characters of the stem>/<stem>.pdf`,
for example:

```sh
ls ~/home-still/papers/10/
```

If the conversion watcher is running, each download is converted next; see
[convert documents](convert-documents.md). A download that finds no copy
lists every source it tried and what each said.
