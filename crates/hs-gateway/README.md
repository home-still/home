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
- **Service routing** by URL path segment
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
    routes:                    # keys must be scribe, distill or mcp
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
| `/cloud/admin/revoke` | POST | Revoke every token issued to `{"subject": "<device name | oauth:client_id>"}` and drop its registry entries |

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

### Service Registry (access token required)

Backend services register themselves at startup and maintain presence with periodic heartbeats. The proxy queries the registry before falling back to the static `routes` config, so registered services take priority.

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/registry/register` | POST | Server announces itself (token must have scope for the service type) |
| `/registry/deregister` | DELETE | Server removes itself from the registry |
| `/registry/heartbeat` | POST | Server sends periodic heartbeat (every 30s) |
| `/registry/services` | GET | Client queries available servers |
| `/registry/set-enabled` | POST | Enable or disable a server |

**`GET /registry/services` response:**

```json
{
  "services": [
    {
      "service_type": "scribe",
      "url": "http://192.0.2.10:7433",
      "device_name": "big",
      "enabled": true,
      "healthy": true,
      "last_heartbeat_secs_ago": 12,
      "metadata": {}
    }
  ]
}
```

**Registration protocol:** Services use their existing cloud enrollment credentials (the access token minted from the refresh token obtained via `hs cloud enroll`). The token's scopes determine which service types the device may register.

**Ownership.** An entry belongs to the device that registered it. Only that device may overwrite, heartbeat, enable/disable or deregister it (403 otherwise). To take an entry away from a device, `hs cloud revoke --name <device>`. A device may hold 16 entries; the registry holds 256.

**Which URLs are accepted.** The announced URL must be `http(s)://<IP literal>:<port>` with no credentials, path, query or fragment. Private LAN addresses (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7) are allowed — that is where real servers live. Refused: loopback, unspecified, link-local (which covers the 169.254.169.254 cloud metadata endpoint), multicast, broadcast, the AWS IPv6 and Alibaba metadata addresses, and hostnames (a name cannot be checked without resolving it, and can be re-pointed after registration).

**Dynamic routing:** When a proxied request arrives, the gateway first checks the service registry for a healthy, enabled instance of the service. If there is none, it uses the static `routes` entry from `config.yaml`.

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

Every token issued to that subject up to now stops working at once (access and refresh), its registry entries are dropped, and the revocation survives gateway restarts (`cloud-revoked.json`, mode 0600, beside the signing secret). Enrolling again afterwards issues a fresh, working credential.

## Rotating the signing secret

```sh
cd ~/.home-still
mv cloud-secret.key cloud-secret.key.prev       # the old secret
# add to cloud.gateway:  previous_secret_path: /home/<user>/.home-still/cloud-secret.key.prev
systemctl restart hs-gateway                    # creates a new cloud-secret.key
```

New tokens are signed with the new secret; tokens signed with the previous one keep working. After the refresh TTL (7 days by default) has passed, remove `previous_secret_path` and delete the `.prev` file; anything still signed with it then stops working. Dropping the `.prev` file earlier is the "revoke everything" button: every device must re-enroll.

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

`typ` is required. The proxy and the registry accept only `access` tokens; the refresh endpoints accept only `refresh` tokens.

## Build

```sh
cargo build --release -p hs-gateway

# Cross-compile for ARM64 (Raspberry Pi):
cargo build --release --target aarch64-unknown-linux-gnu -p hs-gateway
```
