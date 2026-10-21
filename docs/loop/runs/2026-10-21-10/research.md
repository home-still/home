# Run 2026-10-21-10 — the document that is in the bucket and cannot be read

## Problem, measured

The previous iteration (rc.377) moved the personal store's **objects**
into the corpus's storage, under the `personal/` prefix, and the docs
service now serves them with signed links. The *data* moved. The **code
path** did not: the personal crate still reads and writes its catalog
through raw `std::fs`.

Measured on the fleet, 2026-10-21, with the objects in the bucket:

```
$ hs personal read casey-brittain-medical-history-overview-65004350
  big       patient: Casey Brittain …        (the local files are here)
  two       error: no markdown found for stem 'casey-brittain-…'
  bmb       error: no markdown found for stem 'casey-brittain-…'
  mac_air   patient: Casey Brittain …        (it has its own copy)
```

And the catalog:

```
$ hs personal list
  two       0 entries shown
```

**So a personal document is in the bucket, is served by the docs
service, is findable by the *search* (which goes through the distill
server) — and still cannot be *read* or *listed* on two of the five
hosts.** The store is in the bucket and out of reach at the same time.

The cause is three `std::fs` calls in the personal crate:

| where | what it does |
|---|---|
| `personal/src/services/catalog.rs:86,91` | `read_markdown` reads the file from the local disk |
| `personal/src/services/catalog.rs:138,161` | `read_sidecar` and `walk_sidecars` read and list the catalog from the local disk |
| `personal/src/services/catalog.rs:202,203,242,243` | the catalog is *written* to the local disk |

The **search** already works everywhere, because it goes through the
distill server, which reads the bucket
(`personal/src/services/distill.rs:58`). So the store is half in the
corpus and half on one machine, and the half that is on one machine is
the half that answers "what do you have?".

## Why this ranks first

Against the runner-up — **cross-encoder reranking**, the backlog's first
item, whose GPU blocker has cleared — it wins on the user's own
ranking of harm:

* A cross-encoder **reorders** the results of a search that works.
  Measured on this corpus, 2026-10-21: it moved the expected
  passage's rank on **4 queries better and 3 worse**, for a **5× latency
  increase** (162 → 776 ms median) and a new GPU service, a new model
  and a new failure mode. The sibling crate's own M37 measured the same
  null (+0.78 points, CI [−3.14, +4.71]).
* This is a **document that is not there**: the user asks an agent
  about their medical history, the agent finds the passage, and then
  the agent cannot open the document it found — on two of the five
  hosts. That is a tier above a reordering.

It also beats the backlog's #3 ("source scopes") on evidence: the
scope problem is real but it is a *ranking* problem (the personal
collection answers "risk factors for falls in elderly people" with *The
Base Class for Game Objects in SSN67*), while this is a **missing
document**.

And it is the *other half* of rc.377's own change: the migration moved
the data, and this moves the code path that reads it. The two are one
piece of work; the first was done without the second, which is why the
store is in the bucket and out of reach.

## The technique

Searched the corpus first, and it holds the design twice.

**The interface/implementation rule.** *A Philosophy of Software
Design*, §7.4 "Interface versus implementation": *"the interface of a
class should normally be different from its implementation: the
representations used internally should be different from the
abstractions that appear in the interface."* And the file-system
example: *"the abstraction provided by a file system omits many
details, such as the mechanism for choosing which blocks on a storage
device to use for the data in a given file. These details are
unimportant to users of the file system."*

**The pass-through warning, and the rule that follows.** Same book:
*"Pass-through methods indicate that there is confusion over the
division of responsibility between classes … the interface to a piece
of functionality should be in the same class that implements the
functionality."* A `read_markdown` that reaches for the local disk is a
**pass-through to the wrong implementation**: the caller asked for a
document, and the answer depends on which disk the caller happens to
be standing on.

**The conclusion the book draws**, §9.9: *"The decision to split or
join modules should be based on complexity. Pick the structure that
results in the best information hiding, the fewest dependencies, and
the deepest interfaces."*

So the technique is: **one deep interface — the `Storage` trait the
corpus already has — and the implementation chosen at run time from the
configuration.** Not a second read path, not a local fallback, not a
cache: the *same* `Storage` the academic corpus uses, so the personal
store's reads and the corpus's reads cannot disagree.

**The pattern is already in this repository.** The `paper` crate does
exactly this: `config.build_storage()` returns an `Arc<dyn Storage>`
from the `storage:` section of the config file
(`paper/src/config.rs:215`), and every read and write goes through it.
The personal crate is the one that never got the treatment.

## The change

**The personal store's catalog goes through `Storage`**, the way the
paper crate's does:

1. **`Config` gains a `storage:` section**, read the same way the
   paper crate reads it, and a `build_storage()` that returns
   `Arc<dyn Storage>`. The `storage:` section is **required** for the
   catalog's read and write paths: a store with no configured storage
   says so, rather than quietly reading a local directory that is not
   the corpus.

2. **The catalog's reads and writes go through `Storage`.**
   `read_markdown`, `read_sidecar`, `walk_sidecars` and the catalog's
   write path all become storage calls, with the same key layout the
   migration produced: `personal/<shard>/<stem>.catalog.yaml` and
   `personal/markdown/<shard>/<stem>.md`.

3. **The inbox stays local, on purpose.** It is a *drop folder* — the
   one place an external agent may introduce bytes — and the config
   documents it as such ("absolute values are taken as-is so a user can
   point the inbox at, e.g., an NFS drop folder shared with a phone
   scanner"). That is a local path by design, and it stays one.

**What this is not.** Not a second copy of the corpus. Not a local
fallback when the bucket is unreachable: a store that cannot be read
says so. Not a cache.

## Expected effect

* `hs personal read` and `hs personal list` work on **every** host
  that can reach the bucket, not only on the one with the files;
* the personal store has **one** read path, so a document that is in
  the corpus is in the corpus for every reader;
* the personal catalog and the academic corpus's catalog are
  read the same way, so they cannot drift.

## The proof

Measured on `big`, 2026-10-21, by moving the local personal store
aside (to `personal-local-backup`, so it is reversible) and asking both
binaries for the same document:

```
$ mv /mnt/home-still/personal /mnt/home-still/personal-local-backup

$ hs personal read casey-brittain-medical-history-overview-65004350   # rc.373
error: Error: no markdown found for stem 'casey-brittain-medical-history-overview-65004350'

$ hs personal list                                                   # rc.373
0 entries shown

$ hs personal read casey-brittain-medical-history-overview-65004350   # rc.378
---
patient: Casey Brittain
source: Epic MyChart EHI export (received 2025-04-17)
domain: medical/overview

$ hs personal list                                                   # rc.378
Effects Techniques in Uncharted 3       other   effects-techniques-in-uncharted-3-3302b34d
LBP Engine Requirements and Lighting …  other   lbp-engine-requirements-and-lighting-9a90548d
…
```

**With the local files gone, the old binary finds nothing and the new
one finds everything** — from the bucket, over the network, with the
same command. The local store was then moved back and the old binary
works again, so nothing was lost.

That is the whole defect in one measurement: the objects were in the
bucket, and the code that read them was looking at a disk.

## The honest part

The **inbox** stays local, and so does the *ingest* step that reads
from it: those are a drop folder and the act of ingesting from it. What
moves behind `Storage` is the **catalog** — the document's own
metadata and its converted text — which is what every reader after the
ingest needs.

The migration from rc.377 already put the objects where the new code
will look for them, so this iteration does **not** need another
migration. It needs the *readers* to be pointed at the same place.
