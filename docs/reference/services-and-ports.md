# Services, ports and endpoints

## Ports

| Port | Service | Started by | Set with |
|---|---|---|---|
| 7433 | Scribe server (conversion) | `hs serve scribe` | `--port` |
| 7435 | A second scribe server on the same host (a convention, e.g. an olmocr converter beside the default one) | `hs serve scribe --port 7435` | `--port` |
| 7434 | Distill server (embedding and search) | `hs serve distill` | `--port`, or `distill_server.port` / `HS_DISTILL_PORT` |
| 7440 | Cloud gateway | `hs-gateway` | `cloud.gateway.listen` (required, e.g. `0.0.0.0:7440`) |
| 7445 | MCP server, HTTP transport | `hs serve mcp` | `--port` |
| 7446 | Docs service (signed document links) | `hs serve docs` | `--port` |
| 6333, 6334 | Qdrant REST and gRPC | Docker / Podman | the compose file |
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

## Distill server endpoints

| Endpoint | Method | Token | Description |
|---|---|---|---|
| `/health` | GET | no | Status, the server's own `pid`, device, collection and versions; 503 if the embedder is unusable or Qdrant is unreachable. |
| `/readiness` | GET | no | `ready`, `capacity` (embedder slots), `in_flight`, `reason`; 503 when not ready. |
| `/status` | GET | yes | Collection statistics (points, documents, device). |
| `/distill` | POST | yes | Index one document. |
| `/distill/stream` | POST | yes | Index one document, streaming NDJSON progress. |
| `/search` | POST | yes | Hybrid search with optional filters. `limit` defaults to 10, at most 200; `query` is at most 65,536 bytes. |
| `/exists/{doc_id}` | GET | yes | The document's chunk count. |
| `/docs` | GET | yes | Distinct document ids. |
| `/docs/titles` | GET | yes | The title stored on each document's points. |
| `/docs/markdown-paths` | GET | yes | The markdown storage key carried on each document's points. |
| `/doc/{doc_id}/title` | POST | yes | Write a title onto a document's points (payload only; no re-embedding). |
| `/doc/{doc_id}` | DELETE | yes | Delete a document's points. |
| `/collection/reset` | POST | yes | Drop and recreate a collection. |
| `/collection/hnsw?collection=<name>` | POST | yes | Start the background HNSW index build on a collection. |
| `/scrub-interstitials` | POST | yes | Delete chunks matching publisher interstitial pages. |
| `/embedder/release` | POST | yes | Drop the model of every idle pool slot now (`{"released": n}`); the next embed reloads it. A host GPU broker calls it before handing the card to another tenant. |

Bad input is a 400, a missing dependency a 503, anything else a 500.

## Docs service endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Liveness. |
| `/docs/v1/{stem}/source` | GET | The original document: PDF, HTML or EPUB for `academic`; PDF, EPUB, DOCX, Markdown or text for `personal`. Honours `Range`. |
| `/docs/v1/{stem}/markdown` | GET | The converted markdown, or one page of it with `page=N`. |
| `/s/{code}` | GET | A short link: `302` to the signed link the code stands for, with its fragment. An unknown or expired code is `404`. |

A request is served only with a valid signature in the query string:
`iat` (issued), `exp` (expiry), `sub` (subject), `col` (collection:
`academic` or `personal`), `fmt` (`source` or `markdown`), optional `page`, and
`sig`, an HMAC-SHA256 over the stem and those fields with the key in
`~/.home-still/docs-link.key`. A host that mints links (`hs docs link`, the
MCP `doc_link` tool and the tools that attach links) must hold the same key.

A short link needs no signature of its own: `/s/{code}` redirects to one of the
signed links above, and that link is checked in full. The code is a record
under `short-links/` in the storage backend, written by `hs docs link --short`
or `doc_link` with `short: true` and deleted by `hs docs prune` once the link
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
first start), and a route for each of `scribe`, `distill`, `mcp` and `docs`.
It then probes each backend's `/health` from its own host and logs a warning
for any it cannot reach; an unreachable backend does not stop it. On SIGTERM
or SIGINT, requests in flight get 30 seconds to finish.

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
| `/cloud/admin/invite` | POST | admin key | Create an enrollment code: `{"device_name": "…", "scopes": ["scribe", "distill", "mcp"]}`. |
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
| `/docs/*` | `docs` | none (signed link) |

Any other path is a 404. A path with `.` or `..` segments, empty segments or
encoded separators is a 400 and is not forwarded. The gateway replaces the
caller's `Authorization` header with `HS_BACKEND_TOKEN`, strips headers that
describe the caller, does not follow backend redirects, and streams both
bodies. Errors: 502 backend unreachable, 504 timeout, 413 body too large, 503
over the concurrency limit. An unauthenticated request gets a 401 whose
`WWW-Authenticate` header points at the OAuth metadata.

Each route in `cloud.gateway.routes` takes one URL or a list. The gateway
round-robins requests across a list and skips an instance that refused a
connection for `backend_failure_cooldown_secs`.

## Event subjects

Events travel over NATS JetStream. Each subject belongs to a stream, and each
worker kind shares one durable pull consumer, so several workers split the
messages and an unacknowledged message is delivered again (at-least-once).

| Subject | Stream | Published when | Durable consumer | Consumed by |
|---|---|---|---|---|
| `papers.ingested` | `PAPERS` | a source document is stored (download, inbox, `hs pipeline catch-up`, `hs scribe reconvert`) | `scribe-workers` | `hs scribe watch-events` |
| `scribe.completed` | `SCRIBE` | a conversion's markdown is stored | `distill-workers` | `hs distill watch-events` |

The `DISTILL` stream holds `distill.>` subjects. `hs pipeline events-reset`
deletes the `PAPERS`, `SCRIBE` and `DISTILL` streams and recreates them from
the current configuration, discarding every queued and in-flight event.
