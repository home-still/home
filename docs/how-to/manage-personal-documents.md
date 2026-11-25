# Manage personal documents

Keep medical, financial and other private records in a store that is separate from your research library, and search, read, replace and remove them. The commands live under `hs personal`; the flags are in the [`hs personal`](../reference/cli.md#hs-personal) reference.

## Before you start

- A distill server is running and serves the `personal_docs` collection, which it creates by default. See [Index and search your library](index-and-search.md).
- A scribe server is running if you add PDFs. Other formats convert on your machine: `docx`, `epub`, `md`, `markdown` and `txt`. See [Convert documents](convert-documents.md).
- Ollama runs on the machine where you run `hs personal add`, with the naming model pulled (`qwen2.5:7b` by default), unless you pin both the title and the category.
- The `storage:` section of `~/.home-still/config.yaml` says where the files live. See [`storage`](../reference/configuration.md#storage).
- `personal.distill_url` and `personal.scribe_url` point at your servers when they are not on `localhost`. Every `personal` key is in [`personal`](../reference/configuration.md#personal).

## Add a document

1. Add one file by path:

   ```sh
   hs personal add ~/Downloads/lab-results.pdf
   ```

   The command converts the file, asks the naming model for a title and one category, stores the original, the markdown and a catalog sidecar, and indexes the text. It prints the result:

   ```text
   ingested: stem=lab-results-2024-q1-abcdef01 title="Lab Results: 2024 Q1" category=medical chunks=12
   ```

   The stem is the sanitised title plus the first eight characters of the file's content hash.

2. Pin the category, the title or both, instead of letting the model choose:

   ```sh
   hs personal add tax-2024.docx --category tax
   hs personal add tax-2024.docx --category tax --title "Tax return 2024"
   ```

   The category is one of `medical`, `financial`, `education`, `legal`, `employment`, `tax`, `insurance`, `correspondence` or `other`, and must also be in `personal.categories`. A category outside the list fails before conversion starts. With both flags the command does not call Ollama.

3. A file whose content is already stored is refused. Replace it with `--force`: the new document is stored and indexed first, then the old document, its markdown, sidecar and vectors are removed.

   ```sh
   hs personal add ~/Downloads/lab-results.pdf --force
   ```

An ingest either completes or leaves the store as it was: a failed conversion or index removes the files that call wrote.

## Let an agent add a document

The MCP `personal_add` tool takes a file name, never a path. It looks in the personal inbox, `personal.ingest_inbox` under `<project_dir>/<storage_dir>`, which is `~/home-still/personal/inbox` by default, on the host that runs the MCP server.

1. Put the file in the inbox:

   ```sh
   mkdir -p ~/home-still/personal/inbox
   cp ~/Downloads/lab-results.pdf ~/home-still/personal/inbox/
   ```

2. Ask your agent to add `lab-results.pdf`. It calls `personal_add` with that file name and waits until indexing finishes; a long PDF can take many minutes.

The tool rejects names with a path separator or a leading dot. See [`personal_add`](../reference/mcp-tools.md#personal_add) and [Connect Claude and agents](connect-claude-and-agents.md).

## List, search and read

```sh
hs personal list
hs personal list --category medical -n 10
hs personal search "cholesterol results" --category medical -n 5
hs personal read lab-results-2024-q1-abcdef01
```

`list` shows the most recently ingested documents first, with title, category and stem. `search` is semantic, over the personal collection only. `read` prints the converted markdown of one stem. `--category` accepts any letter case.

Agents use the same store through `personal_search`, `personal_list` and `personal_read`. See [Personal tools](../reference/mcp-tools.md#personal-tools).

## Re-index a document

After the chunker or the embedder changes, rebuild one document's vectors from its stored markdown. The new vectors replace the old ones in place; nothing is deleted first.

```sh
hs personal reindex lab-results-2024-q1-abcdef01
```

## Delete a document

```sh
hs personal delete lab-results-2024-q1-abcdef01
```

This removes the document's vectors from Qdrant and its source, markdown and sidecar from storage. It asks for no confirmation and cannot be undone. A stem that names nothing is an error. Agents cannot delete personal documents.

## See where the store is configured

```sh
hs personal config show
hs personal config path
```

`show` prints the resolved `personal` settings; `path` prints the configuration file's path.

## Open a personal document from a search hit

Hits from `personal_search` carry signed links when document links are configured. A link names the personal collection, so you can also mint one by hand:

```sh
hs docs link lab-results-2024-q1-abcdef01 --collection personal
```

See [Open documents with links](open-documents-with-links.md).

## Check it worked

1. The document is in the list:

   ```sh
   hs personal list
   ```

2. Search finds it, with its category:

   ```sh
   hs personal search "lab results" -n 3
   ```

3. A deleted document is gone: `hs personal read <stem>` fails, and `hs personal search` does not return it.
