# hs

Unified CLI for the home-still research pipeline.

## Subcommands

```
hs paper search    Search 6 academic providers
hs paper download  Download papers by query or DOI
hs paper get       Look up a single paper by DOI

hs scribe init     Bootstrap PDF conversion services
hs scribe convert  Convert a single PDF to markdown
hs scribe watch    Auto-convert PDFs in a watched directory
hs scribe server   Manage scribe Docker services (start/stop/ping/list)
hs scribe status   Show watch daemon status
hs scribe inbox    Sweep / run the manual-download inbox daemon

hs distill init    Set up Qdrant and distill server
hs distill index   Index markdown files into Qdrant
hs distill search  Semantic search across indexed documents
hs distill server  Manage distill server (start/stop/ping)
hs distill status  Show collection statistics
hs distill hnsw enable --collection <name> --yes   Start the background HNSW index build (needs HS_BACKEND_TOKEN; without --yes it prints the plan and exits 1)

hs serve scribe    Run scribe service on this machine (auto-init)
hs serve distill   Run distill service on this machine (auto-init)
hs serve mcp       Run MCP server on this machine (auto-init)

hs status          Live TUI dashboard (pipeline stats, service health)
hs restart         Restart the services running this host's installed binaries

hs upgrade         Self-update binaries (checksum + version verified) + Docker images + restart
hs upgrade --check Check for updates without installing
hs upgrade --force Reinstall even if on latest version
hs upgrade --pre   Include pre-release tags

hs mcp install     Write the home-still MCP server into Claude / OpenCode client configs
hs mcp uninstall   Remove it again

hs cloud init      Initialize this node as a cloud gateway (signing secret + admin key)
hs cloud invite    Generate a one-time enrollment code (--name <device>, --scope <scribe|distill|mcp>, repeatable)
hs cloud revoke    Revoke every token issued to a device (--name <device>, or oauth:<client_id>)
hs cloud enroll    Enroll this device with a remote gateway
hs cloud status    Show cloud connection status
hs cloud token     Print a fresh access token

hs pipeline        Cross-service operations: rebuild, catch-up, purge-*, reap-phantoms, reconvert-failed, events-reset
hs migrate         One-shot data migrations: sharding, move-root-orphans, quarantine-bad-content, canonicalize-doi-stems, drop-local-html
hs openalex        Local OpenAlex catalog (DuckDB): load, load-works, status, query, build-indexes, build-fts
hs personal        Personal-document store

hs config init     Generate default config file
hs config show     Print resolved configuration
hs config path     Print config file path
```

## Exit codes and interruption

Every command exits non-zero when it did not finish its job: a destructive or
batch command (`hs pipeline *`, `hs migrate *`, `hs distill abstracts build`,
`hs scribe inbox sweep`) reports per-item errors and then fails the whole
command if any item failed; `hs scribe watch-events` / `hs distill
watch-events` exit 1 when the event stream ends, so the service manager
restarts them.

Ctrl+C is graceful for those long-running commands: the item in flight
finishes, a summary of what was and was not done is printed, and the command
exits non-zero. A second Ctrl+C exits immediately. `hs scribe inbox run`
(and the `hs status` dashboard) also stop cleanly on SIGTERM.

Bad inputs are not retried forever: an inbox file whose name cannot be a paper
stem is moved to `corrupted/` once, with the reason logged.

## Global flags

| Flag | Description |
|------|-------------|
| `--color auto\|always\|never` | Color output mode |
| `--output text\|json\|ndjson` | Output format |
| `--quiet` | Suppress non-result output |
| `--verbose` | Debug-level output |
| `-y, --yes` | Skip interactive prompts |

## Structure

```
src/
  main.rs          Entry point, tokio runtime, dispatch
  cli.rs           Clap derive: Cli, TopCmd, ConfigAction
  shutdown.rs      Process-wide graceful-shutdown flag (SIGINT / SIGTERM)
  scribe_cmd.rs    hs scribe subcommands
  scribe_inbox.rs  Inbox sweeper / daemon
  distill_cmd.rs   hs distill subcommands
  cloud_cmd.rs     hs cloud subcommands
  serve_cmd.rs     hs serve subcommands (auto-init, --install unit generation)
  installer.rs     The one verified binary installer (hs upgrade, hs mcp install)
  upgrade_cmd.rs   hs upgrade (self-update, compose refresh, health + CUDA check)
  restart_cmd.rs   hs restart / post-upgrade service restart
  mcp_cmd.rs       hs mcp install/uninstall
  mcp_client.rs    MCP-over-HTTP client used by hs status
  pipeline_cmd.rs  hs pipeline
  migrate_cmd.rs   hs migrate
  status_cmd.rs    hs status (ratatui TUI dashboard)
  scribe_pool.rs   Load-balanced scribe client pool
  daemon.rs        PID file management for background processes
  config/
    default.yaml   Embedded default configuration template
```

## Build

```sh
cargo build -p hs                                             # development: version from `git describe`
HS_RELEASE_TAG=v0.0.1-rc.NNN cargo build --release -p hs      # release: version is the tag
```

The `build.rs` bakes the version (including RC tags) into the binary via `env!("HS_VERSION")`; the rules live in `build-support/version.rs`. A `--release` build without a valid `HS_RELEASE_TAG` fails.
