# Use the OpenAlex catalog

Build a local copy of the OpenAlex scholarly catalog from its public snapshot, so you and your agents can search works, follow citations and find authors with no external API calls. The commands live under `hs openalex`; every flag is in the [`hs openalex`](../reference/cli.md#hs-openalex) reference.

## Before you start

- A Linux or macOS host with room for the snapshot and the database. The snapshot is hundreds of gigabytes compressed, and the database is large too. The load and build commands refuse to run on Windows; `status` and `query` work there.
- The `openalex:` section in `~/.home-still/config.yaml` names the database file and the snapshot directory. Both keys are required. See [`openalex`](../reference/configuration.md#openalex).
- Network access to download the snapshot, and, once per host, to install DuckDB's full-text search extension.
- The MCP server runs on this host if you want agents to use the catalog. See [Connect Claude and agents](connect-claude-and-agents.md).

## Download the snapshot

OpenAlex publishes the snapshot in a public bucket that needs no account. Download the JSON Lines copy with the AWS CLI:

```sh
aws s3 sync "s3://openalex/data/jsonl" "$HOME/openalex-snapshot/data/jsonl" --no-sign-request
```

The loader reads gzip-compressed or plain JSON Lines from `<snapshot_dir>/<entity>/updated_date=<date>/part_*`, so `snapshot_dir` is the directory that holds the entity folders (`works/`, `authors/`, `topics/`, …). The bucket layout and release schedule are in the [OpenAlex snapshot documentation](https://help.openalex.org/access/snapshot/).

## Configure the catalog

```yaml
openalex:
  db_path: ~/home-still/data/openalex/openalex.duckdb
  snapshot_dir: ~/openalex-snapshot/data/jsonl
```

Create the database's directory. The loader keeps its checkpoint, `seen_set.bin`, beside the database file:

```sh
mkdir -p ~/home-still/data/openalex
```

## Build the catalog

The load and build commands take the database file exclusively. If the MCP server runs on this host, stop it first, because it holds the file open while it runs:

```sh
sudo systemctl stop hs-serve-mcp
```

Then run the steps in this order. Each prints what it did and exits non-zero on failure.

1. Load the lookup tables you want. Name one entity per command; the entities are `concepts`, `topics`, `domains`, `fields`, `subfields`, `sources`, `institutions`, `funders`, `publishers` and `authors`. Load `topics` and `authors` at least, so that the catalog tools can return topic names and author names:

   ```sh
   hs openalex load topics
   hs openalex load authors
   ```

2. Load the works. This walks the snapshot's partitions newest first and keeps the newest copy of each work, so each work is stored once:

   ```sh
   hs openalex load-works
   ```

   A run that fails on a partition stops there. Fix the cause and run it again: it resumes. To load a single partition, name it:

   ```sh
   hs openalex load-works --partition updated_date=2024-01-01
   ```

3. Build the secondary indexes:

   ```sh
   hs openalex build-indexes
   ```

4. Install DuckDB's full-text search extension. This needs network access once per host and again after a DuckDB version change:

   ```sh
   hs openalex install-fts
   ```

5. Build the full-text index over titles and abstracts. This step never downloads the extension, so it fails until step 4 has run:

   ```sh
   hs openalex build-fts
   ```

6. Start the MCP server again, so it sees the finished catalog:

   ```sh
   sudo systemctl start hs-serve-mcp
   ```

The MCP server checks for the finished catalog once, at start-up, and offers the `openalex_*` tools only when it is complete. Restart it after every build. See [tool visibility](../reference/mcp-tools.md#tool-visibility).

## Look at the catalog

Count the rows in each table, and see any partition that failed or skipped records:

```sh
hs openalex status
```

Run a read-only SQL `SELECT`:

```sh
hs openalex query "SELECT COUNT(*) FROM works"
hs openalex query "SELECT openalex_id, title, cited_by_count FROM works ORDER BY cited_by_count DESC LIMIT 5"
```

`status` and `query` open the database read-only, so they run while the MCP server is up.

## Use it from an agent

Five tools read the catalog: `openalex_search`, `openalex_get`, `openalex_references`, `openalex_citations` and `openalex_authors_by_topic`. Ask your agent to search for a topic, then to list what a work cites or what cites it. The parameters are in [OpenAlex tools](../reference/mcp-tools.md#openalex-tools).

## Load a newer snapshot

The loader refuses a partition that is newer than one it already loaded for the same entity, because the first copy of each record wins. To take a newer snapshot, point `openalex.db_path` at a new file in a new directory (its checkpoint sits beside it), download the new snapshot, and run the build steps again from the start.

## Check it worked

1. `hs openalex status` lists non-zero row counts for `works` and the lookup tables you loaded, and no failed entries.
2. A query returns a work:

   ```sh
   hs openalex query "SELECT openalex_id, title FROM works LIMIT 1"
   ```

3. After you restart the MCP server, `openalex_search` appears in your client's tool list, and a search returns works. If it does not appear, the catalog is not complete: rerun `hs openalex build-fts`, then restart the server.
