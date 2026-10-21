# Evaluation — the document that is in the bucket and cannot be read

Measured on the live corpus on `big`, 2026-10-21.

## The problem

The previous iteration (rc.377) moved the personal store's **objects**
into the corpus's storage, under the `personal/` prefix, and the docs
service now serves them with signed links. The *data* moved. The
**code path** did not: the personal crate still read and wrote its
catalog through raw `std::fs`.

Measured on the fleet, with the objects already in the bucket:

```
$ hs personal read casey-brittain-medical-history-overview-65004350
  big       patient: Casey Brittain …        (the local files are here)
  two       error: no markdown found for stem 'casey-brittain-…'
  bmb       error: no markdown found for stem 'casey-brittain-…'
  mac_air   patient: Casey Brittain …        (it has its own copy)

$ hs personal list
  two       0 entries shown
```

**So a personal document was in the bucket, was served by the docs
service, was findable by the *search* (which goes through the distill
server) — and still could not be *read* or *listed* on two of the five
hosts.** The store was half in the corpus and half on one machine, and
the half that was on one machine was the half that answered "what do
you have?".

The cause is three `std::fs` calls in the personal crate:

| where | what it does |
|---|---|
| `personal/src/services/catalog.rs:86,91` | `read_markdown` reads the file from the local disk |
| `personal/src/services/catalog.rs:138,161` | `read_sidecar` and `walk_sidecars` read and list the catalog from the local disk |
| `personal/src/services/catalog.rs:202,203,242,243` | the catalog is *written* to the local disk |

## The change

**The personal store's catalog goes through `Storage`**, the way the
paper crate's does:

* `Config` gains a `storage:` section, read from the **top-level**
  `storage:` section of the config file — the same one the corpus uses
  — and a `build_storage()` that returns `Arc<dyn Storage>`.
* The catalog's reads and writes go through `Storage`, at the keys the
  migration already produced: `personal/<shard>/<stem>.catalog.yaml` and
  `personal/markdown/<shard>/<stem>.md`.
* The **ingest** writes its objects to the store too, so a newly
  ingested document lands where the readers look.
* The **inbox stays local, on purpose** — it is a *drop folder*, the
  one place an external agent may introduce bytes, and the config
  documents it as such ("absolute values are taken as-is so a user can
  point the inbox at, e.g., an NFS drop folder shared with a phone
  scanner"). That is a local path by design, and it stays one.

**The pattern is the repo's own.** The `paper` crate has had
`config.build_storage()` returning `Arc<dyn Storage>` from the
`storage:` section all along; the personal crate is the one that never
got the treatment.

## Result

**The proof, by removing the local files.** On `big`, the local personal
store was moved aside (reversibly) and both binaries were asked for the
same document:

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
A Comparative Study of Screen-Space …  other   a-comparative-study-of-screen-space-amb-…
```

**With the local files gone, the old binary finds nothing and the new
one finds everything** — from the bucket, over the network, with the
same command. The local store was then moved back and the old binary
works again, so nothing was lost.

| | before | after |
|---|---|---|
| `hs personal read` with no local files | **"no markdown found"** | **the document** |
| `hs personal list` with no local files | **"0 entries shown"** | **50 entries** |
| the store's read path | the local disk | the corpus's storage |

**A configuration that would have silently split the store is now
refused.** The shared `ConfigFile::default_storage()` refuses an
implicit storage root that differs from `home.project_dir`, and the
error names the fix:

```
no `storage:` section, so objects (papers, markdown, catalog) would be
stored under /home/ladvien/home-still while `home.project_dir` is
/srv/hs; add an explicit `storage:` section stating the root:

storage:
  backend: local
  local:
    root: /srv/hs
```

## The tests

**The personal crate** (`personal/src/services/catalog.rs`,
`personal/src/services/ingest.rs`): the library's 51 tests pass, and
they were updated to write through the store — the sidecar helper and
the document helper both put their objects at the storage keys the
readers use, so a test that passes is a test of the real layout.

**The configuration** (`personal/tests/config_load.rs`): the store's
paths live under the project directory, the storage's root is the
project directory, and a project directory **without** a `storage:`
section is refused.

**The behaviour that had to survive**: the same content under a new
title is still a duplicate, a failing index call still does not cost
the document its vectors, an abandoned ingest still cleans up after
itself, and `--force` still replaces the old stem with the new
ingest. Those four tests are the ones that caught this change being
wrong while it was being written.

## Timings

The change adds a network round trip where there was a disk read, and
removes a disk read where there was a network round trip. Measured on
`big`, with the local files moved aside (so every read is a bucket
read):

* `hs personal read` — the old binary: an error, no data. The new
  binary: **the document**, in **under 3 s** including the process
  start.
* `hs personal list` — the old binary: "0 entries shown". The new
  binary: **50 entries**, one `list` call plus one `get` per sidecar.

The list is the worst case — one request per document — and it is
bounded by the store's size (50 documents), not by the corpus's.

## How the numbers were produced

```sh
# the proof
ssh big 'mv /mnt/home-still/personal /mnt/home-still/personal-local-backup'
ssh big '~/.local/bin/hs personal read <stem>'      # rc.373: not found
ssh big '/tmp/hs-new personal read <stem>'          # rc.378: the document
ssh big 'mv /mnt/home-still/personal-local-backup /mnt/home-still/personal'
```

The live services were not upgraded or restarted: this iteration's
rules say not to run `hs upgrade` on live services. The new binary was
run by hand against the live store, and the local files were moved
back afterwards.
