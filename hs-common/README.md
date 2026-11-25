# hs-common

The shared library of the home-still workspace: storage, the event bus, the
catalog, configuration loading, authentication, signed document links,
pipeline status and the helpers every binary uses. Most of it sits behind
feature flags so each crate compiles only what it needs.

## User documentation

- [Configuration](../docs/reference/configuration.md): the `storage:`,
  `events:` and `logs:` sections this crate loads.
- [Architecture](../docs/explanation/architecture.md): storage, the catalog,
  the event bus and the trust model.
- [Services and ports](../docs/reference/services-and-ports.md#event-subjects):
  event subjects and durable consumers.

## Code map

| Feature | Modules |
|---|---|
| `cli` | `global_args`, `styles`, `tty_reporter` |
| `http` | `http`: the one `reqwest` client builder |
| `service` | `service::protocol`, `service::pool` (least-loaded server pool), `service::inflight`, `service::lib_bootstrap` |
| `gpu-async` | TTL-cached async `gpu::*_async` accessors |
| `catalog` | `catalog`: catalog entries and their stamp helpers |
| `storage` | `storage` (`Storage`, `LocalFsStorage`, `StorageConfig`), `markdown` |
| `storage-s3` | `storage::S3Storage` (Garage and other S3 stores) |
| `events` | `event_bus` (`EventBus`, `EventBusConfig`, the consumer specs), `inbox` (with `storage`), `panic_guard` |
| `events-nats` | `event_bus::NatsBus` (JetStream streams and durable consumers) |
| `logging` | `logging`: spool, shipper and `init` |
| `catch-panic` | `panic_guard::http::catch_panic` (axum middleware) |
| `compose` | `compose`: Docker / Podman Compose detection |
| `auth` | `auth::token` (typed HMAC tokens), `auth::client` (refreshing client), `auth::backend` (`HS_BACKEND_TOKEN`) |
| `docs` | `docs`: the signer, verifier, revocation list and short-link alias table of document links (implies `storage`) |

Always available, without a feature:

| Module | Contents |
|---|---|
| crate root | `resolve_project_dir`, `resolve_log_dir`, path constants, `sharded_key` / `sharded_path` / `validate_stem` (the one stem-to-key mapping) |
| `config_file` | The `~/.home-still/config.yaml` loader and unknown-key reporting. |
| `secrets` | The `~/.home-still/secrets.env` loader. |
| `status` | Pipeline counters, failure kinds and repair listings. |
| `cuda_libs` | Locating the CUDA libraries ONNX Runtime needs. |
| `gpu` | GPU name, memory and per-process VRAM via `nvidia-smi`. |
| `hardware_profile` | Host classes and their default concurrency. |
| `html` | HTML shape detection and paywall-stub heuristics, shared by the downloader and the scribe watcher. |
| `quality` | The indexability floor: whether text carries enough content to embed, shared by conversion and indexing. |
| `mode`, `reporter`, `pipe_reporter` | Output-mode detection and progress reporting. |
| `exit_codes` | The exit codes every binary uses. |

## Build and test

```sh
cargo test -p hs-common --all-features
```

Feature-gated tests run only with their feature; `--all-features` runs them
all.
