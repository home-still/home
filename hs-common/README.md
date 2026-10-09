# hs-common

Shared infrastructure library for the home-still workspace. Most functionality is behind feature flags to keep individual crate dependency trees minimal.

## Features

| Feature | Modules | Used by |
|---------|---------|---------|
| `cli` | `global_args`, `styles`, `tty_reporter` | `hs` CLI |
| `http` | `http` (`http_client`, `client_builder`) | every crate that builds a `reqwest` client |
| `service` | `service::protocol`, `service::pool`, `service::inflight`, `service::lib_bootstrap` | `hs`, `hs-mcp`, `hs-scribe`, `hs-distill` |
| `gpu-async` | async, TTL-cached `gpu::*_async` accessors | scribe / distill request handlers |
| `catalog` | `catalog` | `hs`, `hs-mcp`, `hs-distill`, `hs-scribe` |
| `storage` | `storage` (`Storage`, `LocalFsStorage`, `StorageConfig`), `markdown` | everything that reads or writes papers, markdown, catalog rows |
| `storage-s3` | `storage::S3Storage` (Garage / S3, path-style) | hosts whose `storage.backend` is `s3` |
| `events` | `event_bus` (`EventBus`, `NoOpBus`, `EventBusConfig`), `inbox` (with `storage`), `panic_guard` (also with `logging` or `catch-panic`) | publishers and watchers |
| `events-nats` | `event_bus::NatsBus` (JetStream) | hosts whose `events.backend` is `nats` |
| `logging` | `logging` (spool, shipper, `init`) | every binary |
| `catch-panic` | `panic_guard::http::catch_panic` (axum middleware) | HTTP servers |
| `compose` | `compose` | `hs` |
| `auth` | `auth::token`, `auth::client`, `auth::backend` | `hs`, `hs-gateway`, scribe / distill / MCP servers |

## Key modules

### `auth::token`
HMAC-SHA256 compact tokens. Every token carries a `typ` claim (`TokenType::Access` or `Refresh`); `validate_token` takes the expected type and a slice of verification secrets (current first, previous during a rotation grace period). Used by the gateway (issuing/validating) and CLI clients (storing/refreshing).

```rust
let secret = token::generate_secret();
let claims = TokenClaims {
    sub: "device".into(), iat, exp,
    scope: vec!["scribe".into()],
    typ: TokenType::Access,
};
let token = token::create_token(&secret, &claims)?;

let claims = token::validate_token(&[&secret], &token, TokenType::Access, false)?;
```

### `auth::client`
`AuthenticatedClient` — wraps reqwest with automatic token refresh. Reads credentials from `~/.home-still/cloud-token`, caches access tokens in memory, refreshes transparently when expired.

### `auth::backend`
`BackendToken` — the shared `HS_BACKEND_TOKEN` between the gateway and the servers it proxies to; `from_env()` / `check_authorization()`.

### `service::protocol`
`ServiceClient` trait, NDJSON stream parsing (`NdjsonSplitter`, `StreamLine`), `ReadinessInfo` for load-balanced server selection.

### `service::pool`
`ServicePool<C>` — generic load-balanced server pool. Queries all servers for readiness, picks the least-loaded one, retries on failure.

### `storage`
The `Storage` trait (`get`, `put`, `head`, `list`, `delete`, `exists`, `ensure_ready`) with a `LocalFsStorage` and an `S3Storage` backend, selected by the `storage:` config section (`StorageConfig::build`). Every key is checked by `validate_key` on every operation (no `..`, absolute or backslash keys). `LocalFsStorage::put` writes a temp file and renames it, so readers never see a partial object. `is_not_found` / `is_invalid_key` classify errors for event handlers. `S3Storage` gives every request a fixed 15-minute total timeout (not configurable; connect timeout and retries keep object_store's defaults), so a stalled peer fails the request instead of hanging it. `storage.local.root`, `home.project_dir` and `home.log_dir` must be absolute after `~/` expansion (`~\` too on Windows); a relative value is a config error naming the key. An unparsable `RUST_LOG` is a startup error (`invalid RUST_LOG `<value>`: ...`); a valid one overrides the stderr level, except under `--quiet`.

### `catalog`
`CatalogEntry` — YAML-serialized paper metadata with conversion info, page offsets, and file references. Read and written through `Storage`: `read_catalog_entry_via()`, `write_catalog_entry_via()`, `update_conversion_catalog_via()` and the other `*_via` stamp helpers, `list_catalog_entries_via()` / `list_catalog_entries_parallel()`.

### `event_bus`
`EventBus` publish / pull-consume with explicit `ack` / `nak` / `term`. A handler that outlives the consumer's `ack_wait` keeps its event alive with `Event::in_progress()`; `with_progress_heartbeat(&event, progress_interval(ack_wait), handler)` sends it every `ack_wait / 3` for as long as `handler` runs (both watchers use it). `events.backend` has no default: `EventBusConfig::build_required` fails when the section is missing, and `noop` must be named. `NatsBus` provisions the `PAPERS`, `SCRIBE` and `DISTILL` JetStream streams and the durable consumers.

### `compose`
`ComposeCmd` — auto-detects Docker Compose, Podman Compose, or standalone variants. Methods: `run()`, `run_silent()`, `run_capture()`, `exec_run()`.

## Always-available

These modules are available without any feature flag:
- `resolve_project_dir()` / `resolve_log_dir()` — config-aware path resolution
- `HIDDEN_DIR` / `CONFIG_REL_PATH` / `PROJECT_DIR_DEFAULT` — path constants
- `sharded_key()` / `sharded_path()` / `validate_stem()` — the one stem-to-key mapping and its untrusted-input check
- `config_file` — `~/.home-still/config.yaml` loader
- `secrets` — `~/.home-still/secrets.env` loader
- `status` — pipeline counters and repair listings
- `mode` — output mode detection (Rich/Plain/Pipe)
- `reporter` / `pipe_reporter` — progress reporting traits
- `exit_codes` — standard exit codes
