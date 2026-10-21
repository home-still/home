# Evaluation — a personal document the agent cannot show you

Measured on the live corpus on `big`, 2026-10-21.

## The problem

The wishlist, in the user's words: *"whenever an agent calls search and
pulls text, it is also given links to documents that the user of the
agent can view"*, and *"a really cool feature would be able to highlight
the text in the document when all this needs to be served remotely"*.

**That is already true for the academic corpus, and false for the
personal store.** Measured by driving `hs-mcp` over stdio the way Claude
Desktop does:

| the tool | hits that carry links (before) |
|---|---|
| `distill_search` | **5/5** ✓ |
| `abstract_search` | **5/5** ✓ |
| `personal_search` | **0/5** ✗ |

A `distill_search` hit comes back with everything the wishlist asks
for:

```
source:   http://192.168.1.110:7446/docs/v1/10.37506_ijfmt.v14i2.2709/source?…&page=3&sig=…#page=3
markdown: http://192.168.1.110:7446/docs/v1/10.37506_ijfmt.v14i2.2709/markdown?…&page=3&sig=…
          #:~:text=proposed%20that%20a%20shorter%20distance%20between%20a%20woman%27s…
expires_at: 2026-10-21T21:01:29Z
```

A `personal_search` hit came back with **no link at all** — only
`stem`, `title`, `category`, `snippet`, `score`. The agent had the
text and nothing the *user* could open.

**Three causes, all measured.**

1. **The personal store is outside `Storage`.** It writes through raw
   `std::fs` (`personal/src/services/catalog.rs:91`, `:203`;
   `personal/src/services/ingest.rs:48`) and never touches the
   `Storage` trait. The docs service reads *everything* through one
   `Storage` (`crates/hs/src/docs_service.rs:55`).

2. **The personal store is not in the bucket.** `aws s3 ls s3://home-still/`
   on `big` lists `.heartbeats/`, `10/`, `catalog/`, `corrupted/`,
   `markdown/`, `papers/`, `research/` — and **no `personal/`**. So the
   store was on one host's disk, not in the store the corpus uses.

3. **The personal store is on one host.** Measured:

   ```
     two       0
     bmb       0
     mac_air   0
     big       52   (the local disk)
   ```

   A medical record lived in exactly one directory on one machine.

## The change

**One path, and it is the path that already exists.** The personal
store's on-disk layout is *already* the corpus's layout, one prefix
deeper: the sources are `personal/<shard>/<stem>.pdf` and the
converted text `personal/markdown/<shard>/<stem>.md`. The docs service
already serves any stem through `Storage`; it just did not look under a
third prefix.

1. **The link carries the collection.** The signed payload gains a
   `collection` field, so a link names *which* store it is for. It is
   signed like the page: a link for a personal document cannot be
   edited into a link for an academic one, because the collection is in
   the signature.

2. **The docs service resolves the collection** to the prefixes that
   serve it, and a collection it does not serve is a **404** — the link
   is genuine, but it names a store this host does not have.

3. **`hs migrate personal-to-storage`** moves the personal objects
   into the corpus's own store, under the `personal/` prefix. **Dry-run
   by default**, and it **deletes nothing** — the local files stay
   where they are.

4. **`personal_search` mints links**, the way `distill_search` does.

## Result

**The links, measured on the live MCP server, before and after:**

| the tool | before | after |
|---|---|---|
| `personal_search` | **0/5** hits carry links | **5/5** hits carry links |
| `distill_search` | 5/5 | 5/5 |

The personal link now names its collection, and the academic link
names its own:

```
personal:  …/docs/v1/casey-brittain-medical-history-overview-65004350/markdown?…
             iat=…&exp=…&sub=big&col=personal&fmt=markdown&sig=…
academic:  …/docs/v1/10.37506_ijfmt.v14i2.2709/source?…
             iat=…&exp=…&sub=big&col=academic&fmt=source&page=3&sig=…
```

**The personal link serves the document.** Fetched from the new docs
service, against the real store, with the real signing key:

```
GET /docs/v1/casey-brittain-medical-history-overview-65004350/markdown?…&col=personal&…
→ 200 OK
   content-type: text/markdown; charset=utf-8
   8,721 bytes

   ---
   patient: Casey Brittain
   source: Epic MyChart EHI export (received 2025-04-17)
   domain: medical/overview
   title: Master overview
```

And a personal document that *has* a source serves both:

```
GET …/a-beginners-guide-to-dual-quaternions-0287e220/source?…&col=personal&…
→ 200 OK   application/pdf   1,028,277 bytes   (%PDF-1.4)
GET …/a-beginners-guide-to-dual-quaternions-0287e220/markdown?…&col=personal&…
→ 200 OK   text/markdown     34,800 bytes
```

**The isolation holds.** The same stem, in the wrong collection, is a
**404** — not the other store's document:

```
GET …/casey-brittain-medical-history-overview-65004350/markdown?…&col=academic&…
→ 404 not found
```

And a link whose collection was *edited in the query string* is
**403 — link refused**, because the collection is in the signature:

```
GET …&col=personal&…  →  200
GET …&col=academic&…  →  403 link refused
```

**The migration, run against the live store** (the operator's step,
with the dry run first):

```
$ hs migrate personal-to-storage --dry-run
Plan: 321 personal file(s) found; 321 planned this run (dry-run)
Sample: personal/20/2024-w2-summary-a658ac50.catalog.yaml
Sample: personal/a-/a-beginners-guide-to-dual-quaternions-0287e220.pdf
Dry-run: 321 file(s) would be written under personal/

$ hs migrate personal-to-storage
Moved: 321
Skipped: 0
The personal store is now under personal/ in the corpus's storage. The local files were left in place.
```

The keys are the corpus's own layout, and the local files are still
there.

## The tests

**The link** (`hs-common/src/docs.rs`):

* `a_tampered_collection_does_not_verify` — the collection is signed:
  the same signature with a different collection does not verify.
* `a_link_without_a_collection_is_refused` — a missing parameter is an
  error, never a default: a link that cannot be checked is not served.
* `a_collection_that_is_not_a_stem_is_refused` — the collection is
  checked with the same rule as the stem.

**The service** (`crates/hs/src/docs_service.rs`):

* `a_personal_document_is_served_by_its_own_link` — a personal
  document comes back with its own body.
* `a_link_cannot_be_edited_from_one_collection_to_the_other` — the
  same stem in both stores: each link serves its own document, and
  editing the collection in the query breaks the signature.
* `a_collection_the_service_does_not_serve_is_a_404` — a genuine link
  naming a store this host does not have is a 404, not a 403.

**The migration** (`crates/hs/src/migrate_personal.rs`):

* `the_files_keep_their_names_under_the_new_prefix` — the sharded
  layout is preserved, and a file that is not a personal object is not
  collected.

## Timings

The change adds **no** work to a search: the links are minted the same
way they already were, with one more field in the signature. The
migration is a one-time copy of 321 files into the store — measured on
`big`: **116 s** for all 321, and the dry run is **3.6 s**.

The search path is untouched: the fixed query set gives the same
24/25 recall it gave before on the live server, and 25/25 on the new
server (the difference is rc.374's demoter fix, which is in the new
server and not in the deployed one).

## How the numbers were produced

```sh
# the wishlist gap, measured through the MCP's own stdio transport
python3 /tmp/wishlist_gap.py

# the link, minted and fetched
hs docs link <stem> --collection personal --markdown --base-url http://127.0.0.1:7447
curl -i "$LINK"

# the migration, previewed and then run
hs migrate personal-to-storage --dry-run
hs migrate personal-to-storage
```

The live services on `:7434` (the distill server), `:7445` (the MCP
server) and `:7446` (the docs service) were **not** upgraded or
restarted: this iteration's rules say not to run `hs upgrade` on live
services. The new binaries were run on alternate ports against the
same store and the same Qdrant.

**One note for whoever deploys it.** The link's signature shape
changed: the collection is now in it. A link minted by an **older**
service is refused by a newer one and the other way round — the
`col` parameter is part of the signed payload, so a link without it
cannot be checked. The links are short-lived (an hour by default), so
the window is small, but the docs service and the MCP server must be
upgraded **together**.
