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

hs distill init    Set up Qdrant and distill server
hs distill index   Index markdown files into Qdrant
hs distill search  Semantic search across indexed documents
hs distill server  Manage distill server (start/stop/ping)
hs distill status  Show collection statistics

hs status          Live TUI dashboard (pipeline stats, service health)

hs upgrade         Self-update binary + Docker images + restart
hs upgrade --check Check for updates without installing
hs upgrade --force Reinstall even if on latest version

hs cloud init      Initialize this node as a cloud gateway
hs cloud invite    Generate a one-time enrollment code
hs cloud enroll    Enroll this device with a remote gateway
hs cloud status    Show cloud connection status
hs cloud token     Print a fresh access token

hs config init     Generate default config file
hs config show     Print resolved configuration
hs config path     Print config file path
```

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
  scribe_cmd.rs    hs scribe subcommands
  distill_cmd.rs   hs distill subcommands
  cloud_cmd.rs     hs cloud subcommands
  upgrade_cmd.rs   hs upgrade (self-update)
  status_cmd.rs    hs status (ratatui TUI dashboard)
  scribe_pool.rs   Load-balanced scribe client pool
  daemon.rs        PID file management for background processes
  config/
    default.yaml   Embedded default configuration template
```

## Build

```sh
cargo build --release -p hs
```

The `build.rs` bakes the full version (including RC tags) into the binary via `env!("HS_VERSION")`.
