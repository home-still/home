# Run 2026-10-21-09 — a personal document the agent cannot show you

## Problem, measured

The wishlist, in the user's words: *"whenever an agent calls search and
pulls text, it is also given links to documents that the user of the
agent can view"*, and *"a really cool feature would be able to highlight
the text in the document when all this needs to be served remotely"*.

**That is already true for the academic corpus, and false for the
personal store.** Measured on the live MCP server, 2026-10-21, by
driving `hs-mcp` over stdio the way Claude Desktop does:

| the tool | hits that carry links |
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

A `personal_search` hit comes back with **no link at all** — only
`stem`, `title`, `category`, `snippet`, `score`. The agent has the
text and nothing the *user* can open.

**The cause is where the personal store lives.** The personal store
writes through raw `std::fs` (`personal/src/services/catalog.rs:91`,
`:203`, `personal/src/services/ingest.rs:48`) and never touches the
`Storage` trait. The docs service reads *everything* through one
`Storage` (`crates/hs/src/docs_service.rs:55`), with two prefixes:
`papers` for the source and `markdown` for the converted text. The
personal store's objects are under a **third** prefix, `personal/`, in
the same bucket — so the same `Storage` could already reach them; the
service simply does not look there.

**And the store is not in the bucket at all.** Measured on `big`:

```
$ aws s3 ls s3://home-still/
                           PRE .heartbeats/
                           PRE 10/
                           PRE catalog/
                           PRE corrupted/
                           PRE markdown/
                           PRE papers/
                           PRE research/
```

There is **no `personal/` prefix**. The 333 personal files (98 of them
markdown) live only on the NFS mount of the host that serves S3, and
**only on `big`** — `two`, `bmb` and `mac_air` have nothing:

```
  two       0
  bmb       0
  mac_air   0
  big       52   (the local disk)
```

So the personal store is invisible to the docs service, invisible to
the other hosts, and invisible to any backup of the bucket. A medical
record lives in exactly one directory on one machine.

**And the personal search is wrong for academic questions.** The
personal collection is a *separate* Qdrant collection, so a search of
it does not leak into an academic search — the default collection is
`academic_papers` and the MCP's `distill_search` names none. But when
an agent *does* search the personal store, it gets this:

| the query | the personal collection's answer |
|---|---|
| "lithium treatment for catatonia" | *Casey Brittain clinical notes (RTF full text)* |
| "risk factors for falls in elderly people" | *The Base Class for Game Objects in SSN67* |
| "exercise and immune system function" | *Casey Brittain visit summaries and CCDA exchanges* |

The third is right; the first two are a game-engine book and a clinical
note, returned because the personal collection is a **game-development
and personal-notes** corpus, not a medical one, and nothing tells the
agent that.

## Why this ranked first

Against the runner-up — **cross-encoder reranking**, the backlog's
first item, whose GPU blocker has cleared: it moved the expected
passage's rank on **4 queries better and 3 worse** last iteration, for
a **5× latency increase** (162 → 776 ms median), and it is still not
in home-still at all. A cross-encoder reorders the results of a search
that works.

This fixes the other half: a search that works but whose results the
user **cannot open**. The user's own words put it first: *"it would be
really nice that whenever an agent calls search and pulls text, it is
also given links to documents that the user of the agent can view."*
That is done for the academic corpus and not for the personal store,
and the gap is measurable: **0 of 5** personal hits carry a link.

It also beats the backlog's #2 and #3 — "the personal store behind
`Storage`" and "source scopes" — because they are the *same change*.
The backlog lists them as two items; they are one: the personal store
needs to be reachable through the storage the docs service already
uses, and then a link can name it.

## The technique, from the corpus

Searched home-still on itself first, and the corpus holds the design
twice.

**The multi-tenancy table.** *Reusing empirical knowledge during cloud
computing adoption* (in the corpus) states the requirement as a cloud
adoption obstacle, T10 "Isolate tenant": *"define tenant-based
identification and hierarchical access control for tenants and (ii)
separate tenant data using authorization and authentication
mechanisms."* The two halves are exactly what is missing: the
personal store has **neither** a tenant identification in its links nor
its data in the shared storage.

**The MCP security surface.** *Model Context Protocol (MCP): Landscape,
Security Threats, and Future Research Directions* (in the corpus) is
the paper that names the risk class this feature walks into: MCP's
openness "exposes new attack surfaces, such as … tool poisoning …
and rug pulls", and *"an AI agent can only act upon malicious
instructions, when it has tools to act"*. A link that the agent hands
to the user is a *capability* — it must grant access to **one
document**, for **one** purpose, for **a bounded time**, and it must
not be a step from a personal document to the academic corpus.

**Designing Data-Intensive Applications** (in the corpus) supplies the
shape for the second half: the right of access belongs to the data, so
the access decision is made *at the resource*, from what the caller
presents — not from a global setting. The signed link already does
this: the stem, the format, the page, the subject and the expiry are
all in the signed payload, so a link is a capability for one resource.

**So the technique is: one signed-link scheme, one storage, and the
collection named in the signed payload.**

## The change

**One path, and the path is the existing one.** The docs service
already serves any stem through `Storage`; the personal store's
objects are already in the same bucket under a third prefix; the
personal store's on-disk layout is *already* the corpus's layout
(`<shard>/<stem>.md`, `<shard>/<stem>.catalog.yaml`).

So:

1. **The link carries the collection.** The signed payload gains a
   `collection` field, so a link names *which* store it is for. The
   academic links carry `academic`; a personal link carries
   `personal`. A link is a capability for one document in one store —
   it cannot be edited into a link for the other store, because the
   collection is in the signature.

2. **The docs service resolves the collection.** The service is told
   the prefixes it serves (`papers`/`markdown` for the academic
   collection, `personal` for the personal one) and resolves the
   signed `collection` to them. A collection it does not serve is a
   404 — there is one path, and it is the one the configuration names.

3. **The personal store moves behind `Storage`.** Its reads and writes
   go through the same trait the corpus uses, so the personal store
   lands in the bucket with the rest of the corpus, on every host, and
   is served by the same docs service. A `hs migrate personal-to-storage`
   moves what is already on disk into the store.

4. **`personal_search` mints links** the way `distill_search` does, so
   the agent hands the user an openable document with the passage
   highlighted.

**What this is not.** Not a second access-control scheme: the
personal documents are served by the *same* signed-link service, with
the collection in the signature. Not a new secret. Not a CPU path, a
fallback, or a second copy of the corpus.

## Expected effect

* `personal_search` returns links with a page anchor and a text
  fragment, like `distill_search` does;
* a personal document is openable by the user, over LAN and remotely,
  with the passage selected — the wishlist's second half;
* the personal store is in the bucket, so it is on every host and in
  the backup;
* a link for a personal document cannot be edited into a link for an
  academic one, or the other way round.

## The honest part

The **migrate** step moves 333 files. The rules say *"Never rebuild,
purge or re-embed the corpus"* — a migration is not a rebuild, but it
does write to the store, so it is a **CLI command, dry-run by
default**, and this iteration does not run it against the live store.
The code change is the deliverable; the migration is run by the
operator, with the same care as every other write.
