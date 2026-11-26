# Open documents with links

Run the docs service, mint signed links to your documents, and open them from your LAN or from anywhere. A link opens one document, at the page you cite, for a limited time. How the pieces fit together is explained in [architecture](../explanation/architecture.md).

## Before you start

- The host that runs the docs service reads the same `storage:` section as the rest of your library, and can reach the storage backend. See [`storage`](../reference/configuration.md#storage).
- That host has libpdfium in `~/.local/lib`, `~/.home-still/dyld-libs` or the system library path. The service renders the pages of a PDF with it and does not start without it.
- Your documents are downloaded and converted. A link to a document that is not in storage returns 404.
- For links that work away from home, a gateway is running with a `docs` route and a public URL such as `https://cloud.example.com`. Set it up with [Build a home cloud cluster](../tutorials/home-cloud-cluster.md); [Access your library remotely](access-remotely.md) covers enrolling devices.

## Run the docs service

1. Run it in the foreground to check that it starts:

   ```sh
   hs serve docs
   ```

   It listens on port 7446 and stops when you press Ctrl+C. On first start it creates the signing key `~/.home-still/docs-link.key` (mode 0600) and never overwrites it.

2. Install it so it starts at boot and restarts on failure. A system service needs `sudo`:

   ```sh
   hs serve docs --install
   ```

   A user service needs none; enable lingering (`loginctl enable-linger`) to keep it running after you log out:

   ```sh
   hs serve docs --install-user
   ```

   Both accept `--port`:

   ```sh
   hs serve docs --install --port 7446
   ```

3. To remove the service, whichever kind you installed:

   ```sh
   hs serve docs --uninstall
   ```

The unit names are in [services and ports](../reference/services-and-ports.md#service-units).

## Set the base URLs

Tell `hs` and the MCP server where links should point. Edit `~/.home-still/config.yaml` on every host that mints links:

```yaml
docs:
  lan_base_url: http://<docs-host>:7446
  remote_base_url: https://cloud.example.com
  link_ttl_secs: 3600
```

The LAN URL goes straight to the docs service, so a large PDF does not travel through the tunnel. The remote URL is the gateway's public URL; the gateway forwards `/docs/*` to the service. See [`docs`](../reference/configuration.md#docs) for every key.

## Use one signing key everywhere

The docs service accepts only links signed with its own key. Every host that mints links, whether through `hs docs link` or the MCP server, signs with the key in its own `~/.home-still/docs-link.key`, and creates a different one when the file is missing. If the MCP server or your shell runs on another host, copy the service's key there, then restart the MCP server on that host:

```sh
scp ~/.home-still/docs-link.key <mcp-host>:.home-still/docs-link.key
ssh <mcp-host> chmod 600 .home-still/docs-link.key
```

On `<mcp-host>`, `hs restart` restarts every home-still service. Skip this step when the docs service, the MCP server and your shell share one host.

## Let the gateway forward links

On the gateway host, add the docs service to `cloud.gateway.routes` and restart the gateway:

```yaml
cloud:
  gateway:
    routes:
      docs: http://<docs-host>:7446
```

The gateway refuses to start when any of `scribe`, `distill`, `mcp` or `docs` has no route, and names the missing key. See [`cloud.gateway`](../reference/configuration.md#cloudgateway).

Then open the port on the docs host to your LAN, so the gateway can reach it:

```sh
sudo ufw allow from <lan-cidr> to any port 7446 proto tcp
```

At start-up the gateway probes each backend from its own host and warns about one it cannot reach. A timeout there usually means a firewall is dropping the port.

## Get links from an agent

The [`read`](../reference/mcp-tools.md#read) tool returns a `links` object with a LAN URL and a remote URL for the source and for the markdown. The [`cite`](../reference/mcp-tools.md#cite) tool checks a quote against a document and returns fresh links that open the document on that passage. Both need `docs.lan_base_url` and `docs.remote_base_url`. See [Connect Claude and agents](connect-claude-and-agents.md).

A PDF link carries `#page=N`. A markdown link from `cite` carries a text fragment (`#:~:text=…`) that selects the passage. The page is part of the signature: a link minted for page 11 opens page 11 and nothing else. For a quote whose words were found on a PDF page, `cite` also returns a `viewer` link and a `page` link, which show the page with the passage boxed.

## Mint a link by hand

Give `hs docs link` a document's stem, the file name without its extension:

```sh
hs docs link 10.1371_journal.pone.0038469
```

It prints the LAN URL and when the link expires. The URLs go to stdout, one per line, and the expiry to stderr, so the URL can be piped or captured: `hs docs link <stem> | xclip`. Add `--remote` for the gateway URL as well, `--markdown` for the converted text instead of the original, and `--page` to open a page:

```sh
hs docs link 10.1371_journal.pone.0038469 --markdown --remote
hs docs link 10.1371_journal.pone.0038469 --page 11
```

`--ttl-secs` overrides `docs.link_ttl_secs` for one link, and `--base-url` overrides `docs.lan_base_url`:

```sh
hs docs link 10.1371_journal.pone.0038469 --ttl-secs 600
```

`--remote` prints a second URL only when `docs.remote_base_url` is set. The command needs `docs.lan_base_url` or `--base-url`. Every flag is in the [`hs docs link`](../reference/cli.md#hs-docs-link) reference.

## Get a short link

The full link carries the stem, six query parameters and a 64-character signature — roughly 170 characters before a text fragment. A **short link** is a 12-character code that redirects to the same signed URL:

```sh
hs docs link 10.1371_journal.pone.0038469 --short --remote
```

```text
http://<docs-host>:7446/s/k7f3m2q9xd4t
https://cloud.example.com/s/k7f3m2q9xd4t
```

Nothing about the authorization changes: the code is an alias stored in your storage backend, and the docs service answers it with a redirect to the signed link, which is still checked in full. The code's `Location` is a path, so one code works both on the LAN and through the gateway; whatever fragment the full link would have carried — `#page=11`, or a `#:~:text=…` passage — travels with the redirect and survives the hop.

An agent gets the same thing from `cite` with `"short"` set to the link the code should stand for (`"source"`, `"markdown"`, `"viewer"` or `"page"`), which returns the long links *and* a `short` object with `lan`, `remote` and `code`.

Notes:

- **Minting one is a write.** The code is a record under `short-links/` in your storage backend, so the host needs the `storage:` section. Search hits never mint one; only `--short` or `cite`'s `short` does.
- **It expires with the link behind it.** A code is answered while its signed link works and is a 404 afterwards, so a link that reached a chat log does not stay clickable past its TTL.
- **Prune the expired ones.** `hs docs prune` deletes every expired record and prints how many it removed. Nothing runs it for you; a weekly cron entry is enough:

  ```sh
  hs docs prune
  ```

## Link to a personal document

The documents you add with `hs personal add` are a separate store. A link names its store, and the store is signed, so a link for a personal document cannot be edited into a link for an academic one:

```sh
hs docs link annual-checkup-notes-e564ee61 --collection personal
```

The docs service serves a personal document from the `personal/` prefix of your storage backend. A personal document's source is whatever it was ingested as — a PDF, an EPUB, a DOCX, a Markdown file or plain text — so a source link opens that file, not a converted copy. If personal documents sit in `~/home-still/personal/` on one host and are not in the backend, copy them there. `hs migrate personal-to-storage` moves whichever source a document has, including one that was ingested as Markdown. The command leaves the local files in place and skips objects already stored with the same size, so it is safe to repeat:

```sh
hs migrate personal-to-storage --dry-run
hs migrate personal-to-storage
```

Without `--dry-run` the command writes at once. `--limit` caps how many files one run moves. See [Manage personal documents](manage-personal-documents.md).

## Check it worked

1. The docs service answers, and can read storage:

   ```sh
   curl http://<docs-host>:7446/health
   ```

   It prints `ok`. A `storage unreachable` reply means the service cannot read the corpus: check `storage:` on the docs host.

2. A link opens. Mint one, then open the URL it printed in a browser, or print the status code with `curl`, quoting the URL because it contains `&`:

   ```sh
   hs docs link 10.1371_journal.pone.0038469
   curl -sS -o /dev/null -w '%{http_code}\n' "<url>"
   ```

   `200` means the document was served. `403` with the body `link refused` is the one answer for an expired, tampered or wrongly signed link; a `403` for a link you just minted points to a different signing key (see [Use one signing key everywhere](#use-one-signing-key-everywhere)). `404` means the stem is not in storage.

3. The remote URL opens from outside your LAN. From a machine off your network, run the same `curl` against the second URL that `hs docs link --remote` prints. A `502` naming `docs` means the gateway has no route; a timeout means the docs host's firewall drops the port.
