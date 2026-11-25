# hs-gateway

The authenticating reverse proxy that makes a home-still library reachable
from the internet. It runs beside a Cloudflare tunnel agent, checks every
request's credential (a device's bearer token, an OAuth 2.1 token for Claude
Desktop, or a document link's signature), and forwards it to the scribe,
distill, MCP or docs service on your network with the shared backend token
in place of the caller's. It trusts no network property: cloudflared
delivers every request from loopback, so no decision is made from a peer
address or a forwarding header.

## User documentation

- [Run home-still across your home machines](../../docs/tutorials/home-cloud-cluster.md):
  setting up the tunnel and the gateway.
- [Access your library remotely](../../docs/how-to/access-remotely.md):
  enrolling, revoking, rotating the signing secret.
- [Connect Claude and other agents](../../docs/how-to/connect-claude-and-agents.md)
- [`cloud.gateway` configuration](../../docs/reference/configuration.md#cloudgateway)
- [Gateway flags and endpoints](../../docs/reference/services-and-ports.md#gateway)
- [The gateway and trust](../../docs/explanation/architecture.md#the-gateway-and-trust)

## Code map

```text
main.rs            flags, startup checks, graceful shutdown
config.rs          the cloud.gateway section and its validation (every routable service needs a route)
app.rs             the router: OAuth, enrollment, admin, /docs/*, and the authenticated fallback proxy
auth.rs            bearer-token checks, scopes, ROUTABLE_SERVICES
oauth.rs           OAuth 2.1 authorization code + PKCE (S256), metadata, dynamic client registration
enrollment.rs      enrollment codes and the refresh exchange
admin.rs           invite and revoke, admin key only, refused through any proxy
revocation.rs      the revocation list shared with the docs service
proxy.rs           path-segment routing, header stripping, streaming, the docs handler
balancer.rs        round-robin over a route's backends, with a cooldown after refused connections
backend_url.rs     backend URL validation
backend_health.rs  the startup reachability probe
ratelimit.rs       per-endpoint rate limits
store.rs, state.rs persisted codes and clients; shared state
```

Tokens are `base64url(payload).base64url(HMAC-SHA256(secret, payload))`
with a payload of `sub`, `iat`, `exp`, `scope` and a required `typ`
(`access` or `refresh`); the proxy accepts only access tokens and the refresh
endpoints only refresh tokens (`hs-common`'s `auth::token`).

## Build and test

```sh
cargo test -p hs-gateway
cargo build -p hs-gateway
cargo build -p hs-gateway --target aarch64-unknown-linux-gnu   # Raspberry Pi
```
