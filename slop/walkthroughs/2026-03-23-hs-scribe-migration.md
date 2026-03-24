# Walkthrough: hs-scribe (pdf-mash → client/server rewrite)

**Date:** 2026-03-23
**Status:** Planning
**Checkpoint:** 2c090afa70c2de666ff89151beda8e6443487810

## Goal

Migrate `pdf-mash` to `hs-scribe` with a client/server split — heavy ML deps live only on the server, the `hs` binary gets a lightweight HTTP client.

## Acceptance Criteria

- [ ] `crates/hs-scribe/` exists, `pdf_masher/` deleted
- [ ] `cargo check -p hs-scribe` compiles (client feature, no ML deps)
- [ ] `cargo check -p hs-scribe --features server` compiles (full pipeline)
- [ ] `hs scribe test.pdf` works (sends to server, gets markdown)
- [ ] `hs-scribe-server` binary runs and serves `/scribe`, `/health`
- [ ] `hs scribe server list` / `ping` / `add` / `remove` work
- [ ] Config uses `HS_SCRIBE_` prefix, `~/.config/home-still/config.yaml` under `scribe:`
- [ ] Model paths resolve to XDG data dir (not relative)
- [ ] Docker files exist and build
- [ ] CI includes hs-scribe

## Build Order

### Step 1: Scaffold — move crate, clean up dead code
- Move `pdf_masher/pdf-mash/` → `crates/hs-scribe/`
- Update workspace `Cargo.toml` members
- Rename package in `crates/hs-scribe/Cargo.toml`
- Delete broken examples
- Delete hardcoded `.cargo/config.toml`
- Verify: `cargo check -p hs-scribe`

### Step 2: Config refactor
- Rename `PDF_MASHER_` → `HS_SCRIBE_`
- Config path: `~/.config/home-still/config.yaml` (scribe section)
- Add `table_model_path` field
- Add `resolve_model_path()` using XDG data dir
- Fix `processor.rs` hardcoded slanet path
- Verify: `cargo test -p hs-scribe`

### Step 3: Feature gates
- Split `Cargo.toml` deps into `client` vs `server` features
- Gate heavy modules (`models`, `pipeline`, `ocr`, `watch`) behind `server`
- `lib.rs` uses `#[cfg(feature = "server")]`
- Add stub `client` module (empty for now)
- Verify: `cargo check -p hs-scribe` (client only, no ML deps)

### Step 4: Server module
- Add `server.rs` — axum routes: `POST /scribe`, `GET /health`, `GET /info`
- Streaming response for large PDFs
- Server wraps existing `Processor`
- Register in `lib.rs`
- Verify: `cargo check -p hs-scribe --features server`

### Step 5: Server binary
- Add `src/server_main.rs` — clap args for `--host`, `--port`
- Add `[[bin]]` in Cargo.toml with `required-features = ["server"]`
- Verify: `cargo build -p hs-scribe --features server --bin hs-scribe-server`

### Step 6: Client module
- Add `client.rs` — reqwest client, server selection, streaming response
- Server config: list of URLs with health checking
- `ScribeClient::convert(pdf_bytes, mode) -> Stream<String>`
- Verify: `cargo check -p hs-scribe`

### Step 7: Wire into hs CLI
- Add `hs-scribe` dep to `crates/hs/Cargo.toml` (client feature)
- Add `Scribe` variant to `TopCmd` in `cli.rs`
- Create `scribe_cmd.rs` — dispatch for convert/watch/server/init
- Wire dispatch in `main.rs`
- Verify: `cargo check -p hs`

### Step 8: Docker
- `crates/hs-scribe/docker/Dockerfile` (CPU)
- `crates/hs-scribe/docker/Dockerfile.cuda`
- `crates/hs-scribe/docker/docker-compose.yml`

### Step 9: CI updates
- Add hs-scribe client to default test job
- Add server build job (ubuntu, cached ORT)
- Add docker build job

### Step 10: `hs scribe init` command
- Model download with progress
- Pdfium detection with install instructions
- VLM backend health check

## Files to Create/Modify

| File | Action | Step |
|---|---|---|
| `Cargo.toml` (root) | Modify members | 1 |
| `crates/hs-scribe/Cargo.toml` | Rename + feature gates | 1, 3 |
| `crates/hs-scribe/src/lib.rs` | Feature gates | 3 |
| `crates/hs-scribe/src/config.rs` | Rename prefix, fix paths | 2 |
| `crates/hs-scribe/src/client.rs` | NEW | 6 |
| `crates/hs-scribe/src/server.rs` | NEW | 4 |
| `crates/hs-scribe/src/server_main.rs` | NEW | 5 |
| `crates/hs-scribe/src/pipeline/processor.rs` | Fix model path | 2 |
| `crates/hs/Cargo.toml` | Add hs-scribe dep | 7 |
| `crates/hs/src/cli.rs` | Add Scribe variant | 7 |
| `crates/hs/src/scribe_cmd.rs` | NEW | 7 |
| `crates/hs/src/main.rs` | Add dispatch arm | 7 |
| `crates/hs-scribe/docker/*` | NEW | 8 |
| `.github/workflows/ci.yaml` | Update | 9 |

## Known Dragons

- **`ort` crate trait bounds**: ORT v2.0.0-rc.12 has `NonNull<OrtSessionOptions>: Sync` issues. Don't use `?` directly on `Session::builder()` — wrap with `.map_err(|e| anyhow::anyhow!("{e}"))` (existing code already does this).
- **pdfium-render**: `Pdfium::default()` doesn't return Result — it panics or silently fails. The actual error surfaces at `load_pdf_from_file()`.
- **Model path**: Must be absolute or relative to a known base. Relative paths break when CWD changes.
- **Feature gate ordering**: `client` module must not import anything from `server`-gated modules.

---
*Plan created: 2026-03-23*
