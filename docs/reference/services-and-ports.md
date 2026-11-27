# Services, ports and endpoints

## Ports

| Port | Service | Started by | Set with |
|---|---|---|---|
| 7433 | Scribe server (conversion) | `hs serve scribe` | `--port` |
| 7435 | A second scribe server on the same host (a convention, e.g. an olmocr converter beside the default one) | `hs serve scribe --port 7435` | `--port` |
| 7434 | Distill server (chunking and search) | `hs serve distill` | `--port`, or `distill_server.port` / `HS_DISTILL_PORT` |
| 7440 | Cloud gateway | `hs-gateway` | `cloud.gateway.listen` (required, e.g. `0.0.0.0:7440`) |
| 7445 | MCP server, HTTP transport | `hs serve mcp` | `--port` |
| 7446 | Docs service (signed document links) | `hs serve docs` | `--port` |
| 7462 | Memory server (myelin), full surface: the distill server writes and searches through it | `myelin-mcp --serve` | `--serve <addr:port>` |
| 7463 | Memory server (myelin), agent surface: what an agent connects to | `myelin-mcp --serve-agents` | `--serve-agents <addr:port>` |
| 4222 | NATS with JetStream (event bus) | Docker / Podman | `events.nats.url` |
| 3900 | Garage S3 API (object storage) | Docker / Podman | `storage.s3.endpoint` |
| 11434 | Ollama (the default VLM backend for conversion) | Ollama | `HS_SCRIBE_OLLAMA_URL` |

Every key named here is described in [configuration](configuration.md).

## Service units

`--install` registers a service so it starts at boot and restarts on failure.
The `hs` binary that ran `--install` is the one the unit runs.

| Command | Linux | macOS |
|---|---|---|
| `hs serve scribe --install` | system unit `hs-serve-scribe.service` | LaunchAgent `com.home-still.scribe` |
| `hs serve distill --install` | system unit `hs-serve-distill.service` | LaunchAgent `com.home-still.distill` |
| `hs serve mcp --install` | system unit `hs-serve-mcp.service` | LaunchAgent `com.home-still.mcp` |
| `hs serve docs --install` | system unit `hs-serve-docs.service` | LaunchAgent `com.home-still.docs` |
| `hs serve docs --install-user` | user unit `hs-serve-docs.service` | LaunchAgent `com.home-still.serve-docs` |
| `hs serve scribe-watch --install` | user unit `hs-scribe-watch-events.service` | LaunchAgent `com.home-still.scribe-watch-events` |
| `hs serve distill-watch --install` | user unit `hs-distill-watch-events.service` | LaunchAgent `com.home-still.distill-watch-events` |
| `hs scribe inbox install` | user unit `home-still-scribe-inbox.service` | LaunchAgent `io.home-still.scribe-inbox` |

System units live in `/etc/systemd/system/` and run as the user who
installed them; installing one uses `sudo`. User units live in
`~/.config/systemd/user/` and need no `sudo`; to keep them running after you
log out, enable lingering (`loginctl enable-linger`). LaunchAgents live in
`~/Library/LaunchAgents/` and log to `~/Library/Logs/home-still/`.

`hs restart` and `hs upgrade` find units by the binary they run, not by name:
any systemd unit or LaunchAgent whose program is an installed home-still
binary is restarted.

## Authentication between services

The scribe server, the distill server and the MCP server's HTTP transport
refuse to start without `HS_BACKEND_TOKEN` (at least 32 visible ASCII bytes,
e.g. `openssl rand -hex 32`). Every request except `GET /health` and
`GET /readiness` must carry `Authorization: Bearer <token>`; anything else
gets a 401. Every host that runs a server or a client of one (`hs`, `hs-mcp`,
the gateway) sets the same value in `~/.home-still/secrets.env`.

## Scribe server endpoints

| Endpoint | Method | Token | Description |
|---|---|---|---|
| `/health` | GET | no | `status` is `ok` when the VLM backend can take work, else `backend_unavailable` or `pdfium_fault` with HTTP 503. Also reports the layout and table models, version, GPU name, utilisation and memory, conversion totals, and the VRAM gate's numbers: free VRAM, the headroom it needs, and the processes holding the card when it refuses. |
| `/readiness` | GET | no | `ready`, `vlm_slots_total`, `vlm_slots_available` (0 while the backend gate is closed) and `in_flight_conversions`. Clients dispatch by these numbers. |
| `/info` | GET | yes | Version and capabilities (`layout`, `tables`). |
| `/scribe/stream` | POST | yes | Multipart upload of one document; streams progress and then the markdown as NDJSON. |
| `/scribe/boxes` | POST | yes | The request body is one PDF; the response is its word-box sidecar (gzip-compressed JSON: every word of every page with its box), read from the text layer, or by the word OCR for a page that has none. Runs one document at a time on the CPU and does not wait for the VLM backend. `415` for a body that is not a PDF, `413` past 256 MiB, `422` with `{"error","code"}` for a PDF that cannot be read, `500` with `{"error"}` when the host cannot do it (a missing OCR model file is named). |

## Distill server endpoints

Every route except `/health`, `/readiness` and `/search` names one store: in the `store` field of the body (`/distill`, `/distill/stream`) or in the `store` query parameter (the others). A missing or unknown store is a 400. The stores are listed under [`distill_server`](configuration.md#stores).

| Endpoint | Method | Token | Description |
|---|---|---|---|
| `/health` | GET | no | Status, the server's own `pid`, device, embedding model and versions; 503 if the embedding model is not on the GPU or myelin is unreachable. |
| `/readiness` | GET | no | `ready`, `capacity` (1: writes are serialized), `in_flight`, `reason`; 503 when not ready. |
| `/status?store=<name>` | GET | yes | The store's statistics (`records_count`, documents, device). |
| `/distill` | POST | yes | Index one document into the store its body names. |
| `/distill/stream` | POST | yes | The same, streaming NDJSON progress. |
| `/search` | POST | yes | Search the stores listed in `stores`; an empty or absent list searches the default stores. `limit` defaults to 10, at most 200; `query` is at most 65,536 bytes. Each hit names its store. |
| `/exists/{doc_id}?store=<name>` | GET | yes | The document's chunk count. |
| `/docs?store=<name>` | GET | yes | Distinct document ids. |
| `/doc/{doc_id}?store=<name>` | DELETE | yes | Delete a document's chunks, and its reference lists with them. |
| `/store/reset?store=<name>` | POST | yes | Delete every document of a store; answers `{"store","deleted"}`. |

Bad input is a 400, a missing dependency a 503, anything else a 500.

## Docs service endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Liveness: storage is readable and pdfium is working. |
| `/docs/v1/{stem}/source` | GET | The original document: PDF, HTML or EPUB for `academic`; PDF, EPUB, DOCX, Markdown or text for `personal`. Honours `Range`. |
| `/docs/v1/{stem}/markdown` | GET | The converted markdown, or one page of it with `page=N`. |
| `/docs/v1/{stem}/page.png` | GET | Page `page=N` of a PDF as a PNG 1,400 pixels wide, with the words `hl` names boxed. |
| `/docs/v1/{stem}/page` | GET | That picture in a page of HTML, with links to the pages before and after, the viewer, the PDF and the converted text. |
| `/docs/v1/{stem}/viewer` | GET | The whole PDF in a viewer that draws it in the browser (pdf.js), scrolled to the words `hl` names or to `page=N`. |
| `/docs/assets/{version}/{file}` | GET | The viewer's scripts and the stylesheet. No signature; `{version}` is a hash of the files, and any other is `404`. |
| `/s/{code}` | GET | A short link: `302` to the signed link the code stands for, with its fragment. An unknown or expired code is `404`. |

A request for a document is served only with a valid signature in the query
string: `iat` (issued), `exp` (expiry), `sub` (subject), `col` (the store the
document is in, such as `academic` or `personal`), `fmt` (`source`, `markdown`, `page.png`, `page` or
`viewer`: the route the link opens, and no other), optional `page`, optional
`hl` and `sig`, an HMAC-SHA256 over the stem and those fields with the key in
`~/.home-still/docs-link.key`. A host that mints links (`hs docs link`, the
MCP `cite` tool and the tools that attach links) must hold the same key.

`hl` is the highlight: up to eight runs of words, each written
`<page>.<first word>-<word after the last>` and joined with `~` (`3.41-58`
is words 41 to 57 of page 3), counted from 0 in the page's word boxes
(`boxes/`, built by `hs scribe watch-events`).
The three page routes exist for a PDF only and answer `404` for any other
source, for a page the PDF does not have, and for an `hl` the word boxes
cannot answer. `page.png` and `page` need `page`. Every link a page view or
the viewer carries is signed by the service for the same subject and expiry
as the link it was opened with.

The service renders pages with libpdfium, which it binds from `~/.local/lib`,
`~/.home-still/dyld-libs` or the system library path, and it refuses to start
without it.

A short link needs no signature of its own: `/s/{code}` redirects to one of the
signed links above, and that link is checked in full. The code is a record
under `short-links/` in the storage backend, written by `hs docs link --short`
or `cite` with `short` and deleted by `hs docs prune` once the link
behind it expires.

The service reads the revocation list `~/.home-still/cloud-revoked.json`,
which `hs cloud revoke` writes on the gateway host, when it starts. A link
whose subject is on that list is refused; restart the service after a
revocation for it to take effect.

## MCP server

`hs serve mcp` runs `hs-mcp --serve 0.0.0.0:<port>`. Clients connect to
`http://<host>:7445/mcp` with the backend token. See
[MCP tools](mcp-tools.md).

## Gateway

```text
hs-gateway --gateway-url https://cloud.example.com [--listen <addr:port>]
```

| Flag | Default | Meaning |
|---|---|---|
| `--gateway-url` | required | The gateway's public `https://` origin, published as the OAuth issuer. Startup fails without it. |
| `--listen` | `cloud.gateway.listen` | The address to listen on. |

At startup the gateway requires `HS_BACKEND_TOKEN`, the signing secret
(`~/.home-still/cloud-secret.key`) and the admin key
(`~/.home-still/cloud-admin.key`; both are created by `hs cloud init`, or on
first start), and a route for each of `scribe`, `distill`, `mcp`, `memory`
and `docs`. It then probes each backend's `/health` from its own host and
logs a warning for any that does not answer or that answers with a server
error; any other answer, including the `401` of a server that wants the
backend token on every path, shows the backend is reachable. A backend that
is away does not stop the gateway. On SIGTERM or SIGINT, requests in flight
get 30 seconds to finish.

## Gateway endpoints

| Endpoint | Method | Authentication | Description |
|---|---|---|---|
| `/.well-known/oauth-protected-resource` | GET | none | RFC 9728 resource metadata. |
| `/.well-known/oauth-authorization-server` | GET | none | RFC 8414 authorization server metadata. |
| `/authorize` | GET, POST | enrollment code | Browser form for the OAuth authorization code flow. PKCE `S256` is required, and the client and its exact `redirect_uri` must be registered. |
| `/token` | POST | none | Code exchange (with PKCE) and token refresh. |
| `/register` | POST | none | Dynamic client registration (RFC 7591). Redirect URIs must be `https`, or `http` on a loopback host. |
| `/health` | GET | none | Returns `ok`. |
| `/cloud/enroll` | POST | enrollment code | Exchange an enrollment code for a refresh token. |
| `/cloud/refresh` | POST | refresh token | Exchange a refresh token for an access token. |
| `/cloud/admin/invite` | POST | admin key | Create an enrollment code: `{"device_name": "…", "scopes": ["scribe", "distill", "mcp", "memory"]}`. |
| `/cloud/admin/revoke` | POST | admin key | Revoke every token issued to `{"subject": "<device name or oauth:client_id>"}`. |
| `/docs/{*rest}` | GET | the link's signature | Proxied to the `docs` route. No bearer token: the signed link is the authorisation. |
| any other path | any | access token | Proxied by its first path segment; see below. |

The OAuth and enrollment endpoints are rate limited. The admin endpoints read
the key in `~/.home-still/cloud-admin.key` and refuse any request that carries
a forwarding header (`CF-*`, `X-Forwarded-*`, `Forwarded`, `X-Real-IP`,
`True-Client-IP`, `CDN-Loop`, `Via`), so they work only from the gateway host
itself.

### Proxied paths

| Path | Service | Scope required |
|---|---|---|
| `/scribe`, `/scribe/*` | `scribe` | `scribe` |
| `/distill`, `/distill/*`, `/search`, `/exists/<id>` | `distill` | `distill` |
| `/mcp`, `/mcp/*` | `mcp` | `mcp` |
| `/memory`, `/memory/*` | `memory` | `memory` |
| `/docs/*` | `docs` | none (signed link) |

A path reaches its backend unchanged, except under `/memory`: the memory
server serves `/mcp`, which on the gateway belongs to the `mcp` service, so
the gateway takes the leading `/memory` off. `/memory/mcp` reaches the
memory server as `/mcp`.

Any other path is a 404. A path with `.` or `..` segments, empty segments or
encoded separators is a 400 and is not forwarded. The gateway replaces the
caller's `Authorization` header with `HS_BACKEND_TOKEN`, strips headers that
describe the caller, does not follow backend redirects, and streams both
bodies. Errors: 502 backend unreachable, 504 timeout, 413 body too large, 503
over the concurrency limit. An unauthenticated request gets a 401 whose
`WWW-Authenticate` header points at the OAuth metadata.

The memory server's agent listener must accept `HS_BACKEND_TOKEN` as its
bearer token when it listens on an address other hosts can reach: start it
with `MYELIN_MCP_TOKEN` set to the same value.

Each route in `cloud.gateway.routes` takes one URL or a list. The gateway
round-robins requests across a list and skips an instance that refused a
connection for `backend_failure_cooldown_secs`.

## Event subjects

Events travel over NATS JetStream. Each subject belongs to a stream, and each
worker kind shares one durable pull consumer, so several workers split the
messages and an unacknowledged message is delivered again (at-least-once).

| Subject | Stream | Published when | Durable consumer | Consumed by |
|---|---|---|---|---|
| `papers.ingested` | `PAPERS` | a source document is stored in any store (download, inbox, `hs pipeline catch-up`, `hs scribe reconvert`) | `scribe-workers` | `hs scribe watch-events` |
| `scribe.completed` | `SCRIBE` | a conversion's markdown is stored; the payload names the document's store | `distill-workers` | `hs distill watch-events` |
| `scribe.boxes` | `SCRIBE` | a PDF's markdown is stored, a PDF is added to the personal store, or `hs scribe boxes catch-up` finds a converted PDF without word boxes | `box-workers` | `hs scribe watch-events` |

The `DISTILL` stream holds `distill.>` subjects. `hs pipeline events-reset`
deletes the `PAPERS`, `SCRIBE` and `DISTILL` streams and recreates them from
the current configuration, discarding every queued and in-flight event.
