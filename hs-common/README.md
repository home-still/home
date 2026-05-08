# hs-common

Shared infrastructure library for the home-still workspace. All functionality is behind feature flags to keep individual crate dependency trees minimal.

## Features

| Feature | Modules | Used by |
|---------|---------|---------|
| `cli` | `global_args`, `styles`, `tty_reporter` | `hs` CLI |
| `service` | `service::protocol`, `service::pool`, `service::inflight`, `service::registry` | `hs`, `hs-mcp` |
| `catalog` | `catalog` | `hs`, `hs-mcp`, `hs-distill` |
| `compose` | `compose` | `hs` |
| `auth` | `auth::token`, `auth::client` | `hs`, `hs-gateway` |

## Key modules

### `auth::token`
HMAC-SHA256 compact token creation and validation. Used by both the gateway (issuing/validating) and CLI clients (storing/refreshing).

```rust
// Create a token
let secret = token::generate_secret();
let claims = TokenClaims { sub: "device".into(), iat, exp, scope: vec!["scribe".into()] };
let token = token::create_token(&secret, &claims)?;

// Validate
let claims = token::validate_token(&secret, &token, false)?;
```

### `auth::client`
`AuthenticatedClient` — wraps reqwest with automatic token refresh. Reads credentials from `~/.home-still/cloud-token`, caches access tokens in memory, refreshes transparently when expired.

### `service::protocol`
`ServiceClient` trait, NDJSON stream parsing, `ReadinessInfo` for load-balanced server selection.

### `service::pool`
`ServicePool<C>` — generic load-balanced server pool. Queries all servers for readiness, picks the least-loaded one, retries on failure.

### `service::registry`
Client-side gateway registry queries. Requires `auth` + `service` features. Discovers running scribe/distill servers from the gateway service registry. Discovery either returns a server list or an error — callers never fall back silently to a default or config-defined pool (ONE PATH).

```rust
// Registry-only discovery — errors propagate.
let servers = discover_servers(&auth, "scribe").await?;
```

### `catalog`
`CatalogEntry` — YAML-serialized paper metadata with conversion info, page offsets, and file references. Functions: `read_catalog_entry()`, `write_catalog_entry()`, `update_conversion_catalog()`.

### `compose`
`ComposeCmd` — auto-detects Docker Compose, Podman Compose, or standalone variants. Methods: `run()`, `run_silent()`, `run_capture()`, `exec_run()`.

## Always-available

These modules are available without any feature flag:
- `resolve_project_dir()` / `resolve_log_dir()` — config-aware path resolution
- `HIDDEN_DIR` / `CONFIG_REL_PATH` / `PROJECT_DIR_DEFAULT` — path constants
- `mode` — output mode detection (Rich/Plain/Pipe)
- `reporter` / `pipe_reporter` — progress reporting traits
- `exit_codes` — standard exit codes
