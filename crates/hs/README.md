# hs

The `hs` command-line tool: every user-facing command of home-still, the
service installers, the `hs status` dashboard, `hs upgrade`, and the docs
service that serves signed document links.

## User documentation

- [`hs` command reference](../../docs/reference/cli.md): every command, flag,
  exit code and example.
- [Run and upgrade services](../../docs/how-to/run-and-upgrade-services.md)
- [`hs status` dashboard](../../docs/reference/status-dashboard.md)
- [Documentation index](../../docs/README.md)

## Code map

```text
src/
  main.rs                 entry point, secrets loading, runtime, dispatch, hs config
  cli.rs                  clap derive: Cli, TopCmd, MigrateAction, ConfigAction
  shutdown.rs             process-wide graceful shutdown (SIGINT / SIGTERM)
  scribe_cmd.rs           hs scribe; the tiered watcher dispatch
  scribe_pool.rs          load-balanced scribe client pool
  scribe_inbox.rs         the inbox sweeper and daemon
  scribe_inbox_install.rs hs scribe inbox install | uninstall | status
  distill_cmd.rs          hs distill
  docs_cmd.rs             hs docs link | prune
  docs_service.rs         the docs service behind hs serve docs
  serve_cmd.rs            hs serve: foreground, start/stop, --install unit and plist generation
  restart_cmd.rs          hs restart and the post-upgrade restart
  upgrade_cmd.rs          hs upgrade: binaries, compose refresh, health and CUDA checks
  installer.rs            the one verified binary installer (hs upgrade, hs mcp install)
  mcp_cmd.rs              hs mcp install | uninstall
  mcp_client.rs           MCP-over-HTTP client used by hs status
  status_cmd.rs           hs status (ratatui dashboard and text output)
  cloud_cmd.rs            hs cloud
  pipeline_cmd.rs         hs pipeline
  migrate_cmd.rs          hs migrate
  migrate_personal.rs     hs migrate personal-to-storage
  openalex_cmd.rs         hs openalex
  daemon.rs               PID-file management for background processes
  docs_tests.rs           drift tests: docs/reference/cli.md and every hs command the docs mention
  test_http.rs            loopback HTTP fixture for unit tests
config/
  default.yaml            the template hs config init writes
```

`hs paper` and `hs personal` are implemented in the `paper` and `personal`
crates; `hs distill`'s argument types live in `hs-distill`.

## Build and test

```sh
cargo build -p hs
cargo test -p hs
```

The version is baked in by `build.rs` (rules in `build-support/version.rs`):
a development build takes it from `git describe`, and a `--release` build
requires `HS_RELEASE_TAG`. See [CONTRIBUTING](../../CONTRIBUTING.md).
