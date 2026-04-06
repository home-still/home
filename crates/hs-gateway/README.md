# hs-gateway

Authenticated reverse proxy for secure remote access to home-still services over the internet.

## Architecture

```
Remote client             Cloudflare Edge              Gateway host            LAN services
  Claude / CLI ──[HTTPS]──> cloud.example.com ──[QUIC]──> hs-gateway ──[HTTP]──> scribe, distill, MCP
                  (TLS)       (tunnel)                   (token auth)            (plain HTTP)
```

The gateway runs alongside your Cloudflare tunnel agent. It validates bearer tokens (or OAuth2 for Claude Desktop), then reverse-proxies requests to backend services on your LAN.

## Features

- **OAuth 2.1 Authorization Code + PKCE** for Claude Desktop remote MCP access
- **HMAC-SHA256 bearer tokens** with automatic refresh (4-hour access, 7-day refresh)
- **Enrollment codes** for one-time device registration (5-minute, single-use)
- **Scope-based authorization** (scribe, distill, mcp)
- **Dynamic Client Registration** (RFC 7591)
- **Service routing** by URL path prefix

## Setup

### 1. Initialize the gateway

```sh
hs cloud init    # generates HMAC signing secret at ~/.home-still/cloud-secret.key
```

### 2. Configure

Edit `~/.home-still/config.yaml`:

```yaml
cloud:
  role: gateway
  gateway_url: https://cloud.example.com
  gateway:
    listen: 127.0.0.1:7440
    secret_path: /home/you/.home-still/cloud-secret.key
    token_ttl_secs: 14400      # 4 hours
    refresh_ttl_secs: 604800   # 7 days
    routes:
      scribe: http://gpu-server:7433
      distill: http://gpu-server:7434
      mcp: http://127.0.0.1:7445
```

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

Or manually: `hs-gateway --gateway-url https://cloud.example.com`

## Endpoints

### OAuth 2.1 (unauthenticated)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/.well-known/oauth-protected-resource` | GET | RFC 9728 resource metadata |
| `/.well-known/oauth-authorization-server` | GET | RFC 8414 auth server metadata |
| `/authorize` | GET/POST | Browser-based enrollment code form |
| `/token` | POST | Code exchange (+ PKCE) and token refresh |
| `/register` | POST | Dynamic Client Registration (RFC 7591) |

### Service (unauthenticated)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Gateway health check |

### Admin (localhost only)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/cloud/admin/invite` | POST | Register enrollment codes |
| `/cloud/enroll` | POST | Exchange enrollment code for refresh token |
| `/cloud/refresh` | POST | Exchange refresh token for access token |

### Proxy (bearer token required)

All other paths are proxied to backend services based on path prefix:

| Path prefix | Service | Scope required |
|-------------|---------|----------------|
| `/scribe/*` | scribe | `scribe` |
| `/distill/*`, `/search`, `/exists/*` | distill | `distill` |
| `/mcp/*` | mcp | `mcp` |

Unauthenticated requests return `401` with a `WWW-Authenticate` header pointing to the OAuth discovery endpoint, triggering the OAuth flow in Claude Desktop.

### Service Registry (bearer token required)

Backend services register themselves at startup and maintain presence with periodic heartbeats. The proxy queries the registry before falling back to the static `routes` config, so registered services take priority.

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/registry/register` | POST | Server announces itself (bearer token must have scope for the service type) |
| `/registry/deregister` | DELETE | Server removes itself from the registry |
| `/registry/heartbeat` | POST | Server sends periodic heartbeat (every 30s) |
| `/registry/services` | GET | Client queries available servers |
| `/registry/set-enabled` | POST | Enable or disable a server |

**`GET /registry/services` response:**

```json
[
  {
    "service_type": "scribe",
    "url": "http://gpu-server:7433",
    "device_name": "big",
    "enabled": true,
    "healthy": true,
    "last_heartbeat_secs_ago": 12,
    "metadata": {}
  }
]
```

**Registration protocol:** Services use their existing cloud enrollment credentials (the same bearer token obtained via `hs cloud enroll`) to authenticate when calling `/registry/register`. The token's scopes determine which service types the device is allowed to register.

**Dynamic routing:** When a proxied request arrives, the gateway first checks the service registry for a healthy, enabled instance matching the path prefix. If no registry entry is found, it falls back to the static `routes` map in `config.yaml`.

## Enrolling devices

**On the gateway host:**

```sh
hs cloud invite --name laptop    # prints enrollment code (e.g., "A7X-K9M")
```

**On the remote machine:**

```sh
hs cloud enroll --gateway https://cloud.example.com
# enter the code when prompted
```

Credentials are saved to `~/.home-still/cloud-token`.

## Claude Desktop (OAuth2 flow)

1. Add `https://cloud.example.com/mcp` as a remote MCP server in Claude Desktop
2. Claude discovers OAuth via `/.well-known/` endpoints and opens your browser
3. The browser shows an enrollment code form
4. Generate a code: `hs cloud invite` on the gateway host
5. Enter the code, click Authorize
6. Claude Desktop stores tokens and auto-refreshes them using the 7-day refresh token

## Token format

```
base64url(payload).base64url(HMAC-SHA256(secret, payload))

payload = {
  "sub": "device-name",
  "iat": unix_timestamp,
  "exp": unix_timestamp,
  "scope": ["scribe", "distill", "mcp"]
}
```

## Build

```sh
cargo build --release -p hs-gateway

# Cross-compile for ARM64 (Raspberry Pi):
cargo build --release --target aarch64-unknown-linux-gnu -p hs-gateway
```
