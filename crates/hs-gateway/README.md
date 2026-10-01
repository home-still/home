# hs-gateway

Authenticated reverse proxy for secure remote access to home-still services over the internet.

## Architecture

```
Remote client             Cloudflare Edge              Gateway host            LAN services
  Claude / CLI ──[HTTPS]──> cloud.example.com ──[QUIC]──> hs-gateway ──[HTTP]──> scribe, distill, MCP
                  (TLS)       (tunnel)                   (token auth)            (plain HTTP)
```

The gateway runs alongside your Cloudflare tunnel agent. It validates bearer tokens (or OAuth2 for Claude Desktop), then reverse-proxies requests to backend services on your LAN.

**Nothing about the network connection is trusted.** cloudflared delivers every internet request from loopback, so the gateway never makes a decision from a peer address or from `X-Forwarded-For` / `CF-*` headers. Every request authenticates with a credential.

## Features

- **OAuth 2.1 Authorization Code + PKCE** (S256 only) for Claude Desktop remote MCP access
- **HMAC-SHA256 bearer tokens**, typed (`access` / `refresh`), with automatic refresh (4-hour access, 7-day refresh)
- **Enrollment codes** for one-time device registration (5-minute, single-use)
- **Scope-based authorization** (scribe, distill, mcp — exact match, no wildcard)
- **Revocation** of a device or OAuth client, and **signing-key rotation** with a grace period
- **Dynamic Client Registration** (RFC 7591)
- **Service routing** by URL path segment, round-robin over one or several backends per service
- Streaming proxy with a concurrency limit, request timeouts and a body-size cap

## Setup

### 1. Initialize the gateway

```sh
hs cloud init    # creates, mode 0600, beside each other:
                 #   ~/.home-still/cloud-secret.key   token-signing secret
                 #   ~/.home-still/cloud-admin.key    admin key for `hs cloud invite` / `revoke`
```

Both files are also created on first gateway start if missing. An existing file that is too short is an **error** — the gateway never regenerates a secret behind your back, because that would invalidate every issued token.

### 2. Configure

Edit `~/.home-still/config.yaml`:

```yaml
cloud:
  gateway:
    listen: 127.0.0.1:7440
    secret_path: /home/<user>/.home-still/cloud-secret.key   # admin key + revocation list live beside it
    token_ttl_secs: 14400      # 4 hours
    refresh_ttl_secs: 604800   # 7 days
    routes:                    # keys: scribe, distill, mcp; a URL or a list of URLs
      scribe: http://gpu-server.example.local:7433
      distill: http://gpu-server.example.local:7434
      mcp: http://127.0.0.1:7445
    # Optional limits (defaults shown):
    # max_concurrent_proxy_requests: 64     # excess requests get 503
    # max_request_body_bytes: 268435456     # 256 MiB, streamed (never buffered)
    # backend_connect_timeout_secs: 10
    # backend_read_timeout_secs: 600        # stalled backend -> 504
    # backend_total_timeout_secs: 3600
    # auth_rate_limit_per_minute: 30        # per endpoint: /cloud/enroll, /authorize, /token, /register
    # previous_secret_path: /home/<user>/.home-still/cloud-secret.key.prev   # key rotation only
```

The gateway's public URL is **not** read from the config: pass it with `--gateway-url`. It must be an explicit `https://` origin; startup fails otherwise, because the value is published as the OAuth issuer and authorization endpoint.

### 3. Add Cloudflare tunnel ingress

In your cloudflared config (e.g., `~/.cloudflared/config.yml`):

```yaml
- hostname: cloud.example.com
  service: http://127.0.0.1:7440
  originRequest:
    connectTimeout: 30s
    keepAliveTimeout: 600s
```

Then `cloudflared tunnel route dns <tunnel-name> cloud.example.com` and restart cloudflared.

### 3b. Provision the backend token

The gateway attaches `Authorization: Bearer $HS_BACKEND_TOKEN` to every proxied backend request and **replaces** the caller's `Authorization` header (a user's gateway token is never forwarded). Startup fails if the variable is unset or shorter than 32 bytes. Generate one value with `openssl rand -hex 32` and add `HS_BACKEND_TOKEN=<64-hex-chars>` to `~/.home-still/secrets.env` (mode 0600) on the gateway host **and** on every backend host — the same value everywhere, set before upgrading the gateway.

### 4. Start the gateway

As a systemd service:

```ini
[Unit]
Description=Home-Still Cloud Gateway
After=network.target

[Service]
Type=simple
User=your-user
ExecStart=/path/to/hs-gateway --gateway-url https://cloud.example.com
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Or manually: `hs-gateway --gateway-url https://cloud.example.com`. The gateway shuts down gracefully on SIGTERM and SIGINT (in-flight requests get 30 s to finish).

## Endpoints

### OAuth 2.1 (unauthenticated, rate limited)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/.well-known/oauth-protected-resource` | GET | RFC 9728 resource metadata |
| `/.well-known/oauth-authorization-server` | GET | RFC 8414 auth server metadata |
| `/authorize` | GET/POST | Browser-based enrollment code form. The client and its exact `redirect_uri` must be registered, and PKCE `S256` is mandatory. |
| `/token` | POST | Code exchange (+ PKCE) and token refresh |
| `/register` | POST | Dynamic Client Registration (RFC 7591). Redirect URIs must be `https`, or `http` on a loopback host. |

The tokens issued by `/token` carry exactly the scopes of the enrollment code the user entered.

### Service (unauthenticated)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Gateway health check |
| `/cloud/enroll` | POST | Exchange enrollment code for a refresh token (rate limited) |
| `/cloud/refresh` | POST | Exchange a **refresh** token for an access token (an access token is refused) |

### Admin (admin key required)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/cloud/admin/invite` | POST | Create an enrollment code: `{"device_name": "...", "scopes": ["scribe", "distill", "mcp"]}`. Only those three scopes can be granted — never `*`. |
| `/cloud/admin/revoke` | POST | Revoke every token issued to `{"subject": "<device name \| oauth:client_id>"}` |

Authenticate with `Authorization: Bearer <contents of cloud-admin.key>`. The admin key is a separate secret from the token-signing secret; a signed token is never an admin credential. A request carrying any `CF-*`, `X-Forwarded-*`, `Forwarded`, `X-Real-IP`, `True-Client-IP`, `CDN-Loop` or `Via` header is refused outright — a legitimate admin call is a direct connection from the CLI on the gateway host and has none. `hs cloud invite` and `hs cloud revoke` read the key from the file, so they only work on the gateway host.

### Proxy (access token required)

Requests are routed by path **segment**:

| Path | Service | Scope required |
|------|---------|----------------|
| `/scribe`, `/scribe/*` | scribe | `scribe` |
| `/distill`, `/distill/*`, `/search`, `/exists/<id>` | distill | `distill` |
| `/mcp`, `/mcp/*` | mcp | `mcp` |

`/mcpfoo` is not `/mcp`; any other path is 404. Paths containing `.`/`..` segments (raw or percent-encoded), empty segments, or encoded separators (`%2f`, `%5c`, `%00`) are rejected with 400 and never forwarded. The path **and query string** are forwarded unchanged.

The proxy streams request and response bodies. It strips hop-by-hop headers and anything that describes the original caller (`X-Forwarded-*`, `Forwarded`, `X-Real-IP`, `CF-*`, `Authorization`, `Host`), does not follow backend redirects, and never echoes a backend address in an error (502 unreachable, 504 timeout, 413 body over the limit, 503 over the concurrency limit).

Unauthenticated requests return `401` with a `WWW-Authenticate` header pointing to the OAuth discovery endpoint, triggering the OAuth flow in Claude Desktop.

### Backends and load balancing

`cloud.gateway.routes` is the **only** source of backend addresses; there is no service registry and backends do not announce themselves. Each service takes one URL or a list. Requests round-robin across the list; an instance that refuses a connection is skipped for `backend_failure_cooldown_secs` (default 10) — passive health only, no heartbeats. If every instance is cooling down, requests still rotate through them.

```yaml
routes:
  scribe:
    - http://gpu-a.example.local:7433
    - http://gpu-b.example.local:7433
  distill: http://gpu-a.example.local:7434
  mcp: http://127.0.0.1:7445
```

Startup fails if `routes` is empty, a key is not `scribe`/`distill`/`mcp`, a list is empty or repeats a URL, or a URL is not `http(s)://host[:port]` (no credentials, path, query or fragment). Loopback and private addresses are fine: routes are written by the operator. To add or remove a node, edit the list and restart the gateway.

## Enrolling devices

**On the gateway host:**

```sh
hs cloud invite --name laptop                 # prints enrollment code (e.g., "A7X-K9M")
hs cloud invite --name viewer --scope mcp     # least privilege: this device can only call mcp
```

**On the remote machine:**

```sh
hs cloud enroll --gateway https://cloud.example.com
# enter the code when prompted
```

The device name and scopes are fixed by the invite; the enrolling device cannot choose its own. Credentials are saved to `~/.home-still/cloud-token`. `hs cloud enroll` refuses a non-https gateway.

## Claude Desktop (OAuth2 flow)

1. Add `https://cloud.example.com/mcp` as a remote MCP server in Claude Desktop
2. Claude discovers OAuth via `/.well-known/` endpoints, registers itself (`/register`), and opens your browser
3. The browser shows an enrollment code form naming the client and where it will return to
4. Generate a code: `hs cloud invite --scope mcp` on the gateway host
5. Enter the code, click Authorize
6. Claude Desktop stores tokens and auto-refreshes them using the 7-day refresh token

Anyone can register an OAuth client, so the consent page shows the client name and the host you will be sent to — check it before entering a code.

## Revoking a device

```sh
hs cloud revoke --name laptop            # a device
hs cloud revoke --name oauth:hs-abc123   # an OAuth client (the subject is oauth:<client_id>)
```

Every token issued to that subject up to now stops working at once (access and refresh), and the revocation survives gateway restarts (`cloud-revoked.json`, mode 0600, beside the signing secret). Enrolling again afterwards issues a fresh, working credential.

## Rotating the signing secret

```sh
cd ~/.home-still
mv cloud-secret.key cloud-secret.key.prev       # the old secret
# add to cloud.gateway:  previous_secret_path: /home/<user>/.home-still/cloud-secret.key.prev
systemctl restart hs-gateway                    # creates a new cloud-secret.key
```

New tokens are signed with the new secret; tokens signed with the previous one keep working. After the refresh TTL (7 days by default) has passed, remove `previous_secret_path` from the config, restart, and delete the `.prev` file. Removing `previous_secret_path` earlier is the "revoke everything" button: every token signed with the old secret stops working and every device must re-enroll. (The gateway refuses to start if `previous_secret_path` is set but the file is missing or too short.)

## Token format

```
base64url(payload).base64url(HMAC-SHA256(secret, payload))

payload = {
  "sub": "device-name",
  "iat": unix_timestamp,
  "exp": unix_timestamp,
  "scope": ["scribe", "distill", "mcp"],
  "typ": "access" | "refresh"
}
```

`typ` is required. The proxy accepts only `access` tokens; the refresh endpoints accept only `refresh` tokens.

## Build

```sh
# --release needs the tag the binary ships as (build-support/version.rs)
HS_RELEASE_TAG=v0.0.1-rc.NNN cargo build --release -p hs-gateway

# Cross-compile for ARM64 (Raspberry Pi):
HS_RELEASE_TAG=v0.0.1-rc.NNN cargo build --release --target aarch64-unknown-linux-gnu -p hs-gateway
```
