# Evaluation — signed, expiring links to the documents

Measured on the live corpus on `big`, 2026-10-20.

## The problem

A search hit names a document the user cannot open. The `pdf_path` it
carries is a storage key on the server's disk:

```json
{ "doc_id": "10.3390_s21103461", "title": null, "authors": [],
  "year": 2021, "doi": null, "score": 0.515873,
  "pdf_path": "papers/10/10.3390_s21103461.pdf",
  "line_start": 214, "line_end": 220, "page": 11 }
```

The CLI prints `PDF: papers/10/10.3390_s21103461.pdf`. A user reading
that cannot open it: the path is on the 3090 box, and the machine that
holds the corpus is rarely the machine the user is sitting at. An agent
off-LAN cannot open it either.

The corpus is ready to be served: **17,684 source objects** under
`papers/` (15,892 PDF, 1,598 HTML), 15,817 markdown objects, 16,940
catalog rows, **45.3 GB** in total.

## The change

A docs service (`hs serve docs`, port 7446) that serves the corpus's
documents over signed, expiring links, a CLI that mints them
(`hs docs link <stem>`), a gateway route that proxies to the service,
and the MCP tools that hand them to agents.

The signature is `HMAC-SHA256(key, "v1|stem|fmt|sub|iat|exp")`, and
every field it covers is in the query string, so changing the stem, the
format, the subject or the expiry invalidates it.

## Result

| | before | after |
|---|---|---|
| a search hit carries a link the user can open | **no** | **yes** |
| `pdf_path` on a search hit (a storage key, unopenable) | present | **removed** |
| RECALL@10 (unchanged queries) | 24/25 = 96 % | 24/25 = 96 % |
| median latency | 138 ms | 143 ms |

**Recall is unchanged.** The change adds no retrieval behaviour: it
adds a link to a hit and removes a path from it. A search that found a
passage before still finds it.

## Verified end to end, on real documents

The live service, against the real corpus.

**A real PDF, with every header a viewer needs:**

```
GET /docs/v1/10.1371_journal.pone.0038469/source?exp=…&sub=big&fmt=source&sig=…
→ 200 OK
   content-type: application/pdf
   content-disposition: inline; filename="10.1371_journal.pone.0038469.pdf"
   content-length: 89310
   accept-ranges: bytes
   etag: "0511a0490d022096d6923f7c1ad5232f"
   last-modified: Sun, 15 Apr 2026 22:26:26 GMT
   content-security-policy: sandbox
   x-content-type-options: nosniff
   referrer-policy: no-referrer
   cache-control: private, max-age=3600
```

The body is a real PDF (`%PDF-1.5`).

**A ranged request — what a PDF viewer seeking to page 200 sends:**

```
GET …/source?…  (Range: bytes=1000-1099)
→ 206 Partial Content
   content-range: bytes 1000-1099/89310
   content-length: 100
```

**The markdown link:**

```
GET …/markdown?…
→ 200 OK
   content-type: text/markdown; charset=utf-8
   content-disposition: inline; filename="10.1371_journal.pone.0038469.md"
```

**Every refusal:**

| what | answer |
|---|---|
| a tampered stem | 403 |
| a tampered expiry | 403 |
| a forged signature | 403 |
| no signature at all | 403 |
| an expired link | 403 |
| a link for the source used on the markdown route | 403 |
| a revoked subject | 403 |
| a path-traversal stem (`..%2F..%2Fetc%2Fpasswd`, `%2Fetc%2Fpasswd`, a raw storage key) | **400**, before storage is read |
| a valid link for a document the corpus does not hold | 404 |

Every refusal is a 403 with the same body (`link refused`), so a client
learns nothing about whether the stem exists, whether the link was
expired or whether the signature was forged.

**It streams.** The corpus's largest object is a 294 MB PDF
(`papers/et/eth-cgl-digital_humans-Pra21b.pdf`):

| | before the request | after a full 308 MB download |
|---|---|---|
| the service's RSS | 51.7 MB | **52.1 MB** |

0.4 MB of growth for a 308 MB download. The acceptance criterion was
"under 64 MB".

## The two bugs the tests caught

Both were in the link's own logic, and both were found by the tests
rather than by reading the code.

**The expiry was off by a second.** The check was `now > expires_at`,
so a link minted with a zero TTL lived for the rest of that second
instead of expiring at once. The comparison is `>=` now.

**A revocation could not be applied.** The link carried no issue time,
so the service compared the *expiry* against the revocation time — which
is not the same thing. A link minted before its subject was revoked has
to be refused, and a link minted after it must not be, because the
subject is a new device wearing the same name. The link now carries
`iat` (issued at), it is part of the signature, and the revocation is
checked against it.

## Where the links come from

- **hs-mcp** — every tool that returns a document carries a `links`
  object: `distill_search`, `abstract_search`, `catalog_read`, and the
  new `doc_link` tool, which mints a fresh link when one has expired.
- **the CLI** — `hs docs link <stem> [--markdown] [--remote]`, on the
  same signing code, so a link minted by hand opens exactly what a
  link minted for an agent opens.

## The `docs:` configuration

```yaml
docs:
  lan_base_url: http://<host>:7446
  remote_base_url: https://<public-host>
  link_ttl_secs: 3600
```

The signing key lives beside the other credentials in
`~/.home-still/docs-link.key`, created with mode 0600 and never
overwritten. A missing `docs:` section is not an error: the tools
still work, they just carry no links, and the server says so at
startup.

## Timings

- `GET /docs/titles` over the whole collection: 8.2 s (one scroll of
  438,863 points, reading two payload fields).
- `POST /doc/{id}/title`: 1–21 ms per document.
- A document served end to end: **1–3 ms** locally, the service's own
  work.
- A ranged request: **2 ms** (the provider is read from the pyke
  cache, and the bytes are the only thing that crosses the wire).
- The search path is untouched: median latency 138 ms before, 143 ms
  after.

## How the numbers were produced

```sh
python3 docs/loop/eval_queries.py --limit 10 --out /tmp/run.json
python3 docs/loop/measure_titles.py --limit 10 --out /tmp/titles.json
```

The end-to-end checks above were run against the live service on
`big`, against the real corpus, with `curl`.
