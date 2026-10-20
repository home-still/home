# Run 2026-10-20-04 — signed, expiring links to the documents

## Problem, measured

An agent that calls `distill_search` gets a passage, a `doc_id` and a
`pdf_path`. The `pdf_path` is a **storage key on the server's disk**:

```json
{ "doc_id": "10.3390_s21103461", "title": null, "authors": [],
  "year": 2021, "doi": null, "score": 0.515873,
  "pdf_path": "papers/10/10.3390_s21103461.pdf",
  "line_start": 214, "line_end": 220, "page": 11, "category": null }
```

The CLI prints it as `PDF: papers/10/10.3390_s21103461.pdf`. A user
reading that cannot open it: it is a path on the 3090 box, and the
machine that has the corpus is not the machine the user is sitting at.
An agent off-LAN cannot open it either. The passage found is real and
the text is quoted, but the user is **one click short of the source**,
and the agent has no way to close that gap: every path it is given is
unopenable.

This is the wishlist's first item, stated twice by the user: "whenever
an agent calls search and pulls text, it also is given links to
documents that the user of the agent can view", and issue #7, "Signed
weblinks to source documents and markdown (LAN + remote) via a docs
service".

## The corpus is ready to be served

| prefix | objects | what |
|---|---|---|
| `papers/` | 17,684 | 15,892 PDF, 1,598 HTML, 192 md, 1 mobi |
| `markdown/` | 15,817 | the converted text |
| `catalog/` | 16,940 | one row per document |
| `web/`, `personal/` | 0 | not populated yet |

45.3 GB of sources, mean 2.6 MB, **largest 294 MB**
(`papers/et/eth-cgl-digital_humans-Pra21b.pdf`). A PDF viewer seeking to
page 200 of a large document issues `Range` requests, so the service
must answer them, and it must not load a 294 MB object into memory.

The corpus's own text says the same thing the issue says, from the
retrieval side. RocketQAv2 (Ren et al. 2021,
`10.18653_v1_2021.emnlp-main.224`, in the corpus) argues the document
representation should be rich enough to match a query; DPR (Chen et
al. 2020, `10.18653_v1_2020.emnlp-main.550`, in the corpus) encodes
`title + body`. The previous iteration put the *title* on the payload.
This one puts the *document* within reach of the user.

## Techniques considered

**A. Return the storage key and let the client resolve it.** Zero new
code. The user still cannot open anything — the key is meaningless
outside the host. Rejected: it is what the tool does today, and it is
the problem.

**B. A `file://` URL.** Same problem, plus it leaks the server's
filesystem layout to the agent. Rejected.

**C. A presigned S3 URL.** The issue already rejects this: the S3
endpoint is a LAN host, so a presigned URL does not work off-LAN. And
it would hand out the bucket's credentials path. Rejected.

**D. A new public "browse the corpus" service with no authentication.**
Rejected: every document in the corpus is somebody's copyrighted
paper, and the corpus is not public. Issue #7's "out of scope" says
"Public, unauthenticated sharing".

**E. A docs service that serves documents over signed, expiring URLs.**
The design in issue #7. Chosen.

The choice among E-variants is about **where the signature is checked**.
Two sub-options:

- **E1. The gateway signs and the backend trusts it.** The gateway
  verifies the signature, then rewrites the request to the backend. One
  check, but the gateway must hold the key and the backend must trust
  the gateway's assertion — so the backend is reachable-but-open on the
  LAN, which is exactly the "trusted LAN bypass" the issue forbids.
- **E2. The backend checks the signature itself.** The signature travels
  with the request, the backend verifies it, and the gateway is a dumb
  pipe. The LAN URL and the remote URL point at the same service and go
  through the same check — no bypass. **Chosen.**

E2 also costs almost nothing at the gateway: the proxy already streams
the request and response bodies and already forwards `Range`,
`Content-Range`, `Content-Length`, `ETag` and `Accept-Ranges`
untouched (the gateway's `filtered_headers` is a *deny* list, not an
allow list). So a `206` from the backend is a `206` from the gateway
with no gateway work at all.

## What the code already gives us

Measured by reading the code, not by assuming it:

- **`Storage::head` returns `etag` and `last_modified`.** `S3Storage::head`
  (`hs-common/src/storage/s3.rs:228`) already fills both. Conditional
  requests need nothing new.
- **The source-key probe exists.** `find_source_key`
  (`crates/hs-mcp/src/main.rs:1012`) already probes `pdf` → `html` →
  `epub` against `papers/<shard>/<stem>.<ext>`. The docs service needs
  the same probe; it is the same rule.
- **A reusable HMAC-SHA256 signer exists.**
  `hs_common::auth::token` already has `Hmac<Sha256>`, `create_token`
  and `validate_token` (which accepts *several* keys, so a rotation
  grace period is already a solved problem there).
- **A private-key-file pattern exists.** `create_private_file`
  (`token.rs:~250`) writes with `create_new(true)` and mode `0600`,
  fsyncs, and hard-links — so it never overwrites. `cloud-secret.key`,
  `cloud-admin.key` and `cloud-revoked.json` already live in
  `~/.home-still/` beside it.
- **The revocation list exists**, at `cloud-revoked.json`, and the
  gateway reads it at startup and checks it on every token
  (`auth.rs:104`).
- **`validate_stem` and `validate_key` exist** and already reject
  `..`, absolute keys, NUL and backslashes. The docs service must use
  them, and must never take a storage key from the client.

## Technique chosen

**A docs service that serves documents over signed, expiring URLs, with
the signature checked by the service that reads the bytes.**

- `hs serve docs` — a new service on the storage-facing host, default
  port 7446. It serves `GET /docs/v1/<stem>/source` and
  `GET /docs/v1/<stem>/markdown`, with `Range` support, and checks an
  HMAC signature on every request. A failed, expired or tampered
  signature is a 403 that says nothing about whether the stem exists.
- The signature is `HMAC-SHA256(docs_link_key, "v1|stem|fmt|sub|exp")`.
  The query string is `exp`, `sub`, `fmt`, `sig`.
- The **key** lives in `~/.home-still/docs-link.key`, created with the
  existing `create_private_file` pattern, and never in `config.yaml`.
- The **TTL** is short by default (1 hour) and the link is bound to
  stem + format + subject, so a leaked link opens one document, for a
  bounded time.
- The **minting** side is a CLI command (`hs docs link <stem>`) and a
  shared library the MCP server calls, so both produce identical
  links.
- The **gateway** gets a `/docs` route that proxies to the docs service
  without requiring a bearer token (the signature *is* the
  authorization) but with the same rate limit and concurrency cap as
  every other proxied path.

### The one thing that makes it work end to end

A link is only useful if the user can open it, so the two URLs must
differ only in how they reach the same service:

- `lan` → `http://<host>:7446/docs/v1/<stem>/source?...` — straight to
  the docs service. A large PDF never hairpins through the tunnel.
- `remote` → `https://<public-host>/docs/v1/<stem>/source?...` — through
  the tunnel and the gateway, which proxies it to the same service.

Both carry the **same signature**, checked by the **same code** in the
**same process**. There is no "trusted LAN" bypass, and there is no
second implementation of the check that could drift.

## Expected effect

- An agent's search hit carries a link the user can click, on the LAN
  and off it, and the link opens the actual PDF (or the markdown) in a
  browser.
- A large PDF is seekable, because the service answers `Range` with
  `206` and the streaming path never holds the whole object.
- The corpus is not exposed: no link works without a valid signature,
  every link expires, and every link is bound to one document.
- `pdf_path` stops being handed to agents as if it were a link.

## Constraint compliance

- No corpus rebuild, no re-embed, no purge, no change to the durable
  consumers. The docs service only *reads* objects that already exist.
- The minting is a CLI command and an MCP tool; the MCP tools stay
  report-only in the sense that matters (they do not mutate the corpus).
- The service is on the storage-facing host, so it reads through the
  same `Storage` trait everything else reads through — one read path,
  not a second one that bypasses it.
- Path traversal is refused by the existing `validate_stem` /
  `validate_key`, and the client never supplies a storage key.
- No GPU is used: serving a PDF needs no embedding model.
