# Walkthrough: Unified `hs` CLI Binary

**Date:** 2026-03-20
**Status:** In Progress
**Checkpoint:** 64a3ef477746c4a7b84732806c439c5547af20e5

## Goal

Create a unified `hs` CLI binary that wraps paper (and later pdf_masher) as subcommands, with CI/CD for cross-platform releases and `curl | sh` installation.

## Acceptance Criteria

- [ ] `cargo run -p hs -- paper search "test"` dispatches to paper's search handler
- [ ] `cargo run -p hs -- --help` shows the `paper` subcommand
- [ ] Paper crate is library-only (no `[[bin]]`)
- [ ] `cargo check` passes for entire workspace
- [ ] `cargo test -p paper` passes (library tests still work)
- [ ] CI workflow exists for lint/test on push
- [ ] Release workflow builds `hs` for 5 platform targets
- [ ] `docs/install.sh` installs `hs` from GitHub releases
- [ ] Paper repo CI/CD deleted

## Technical Approach

### Architecture

The `hs` binary is a thin shell: parse top-level CLI, set up reporter/styles, delegate to domain crate dispatch functions. Each domain crate (paper, later pdf-mash) exports a clap `Subcommand` enum and a `dispatch()` function.

```
hs binary (crates/hs/)
  -> paper::commands::dispatch(PaperCmd, &GlobalArgs, &Reporter, &Styles)
  -> (future) pdf_mash::commands::dispatch(MashCmd, ...)
```

### Key Decisions

- **GlobalOpts eliminated**: paper's `GlobalOpts` was a thin wrapper around `hs_style::global_args::GlobalArgs`. We add `is_json()` and `color_str()` to `GlobalArgs` directly and use it everywhere.
- **Paper exports its CLI types**: `PaperCmd` (clap Subcommand) lives in `paper::cli` so the `hs` crate can embed it via `#[command(subcommand)]`.
- **Dispatch in paper library**: Paper exports `commands::dispatch()` which matches on `PaperCmd` and calls the right handler. This is the single entry point for the hs binary.

### Files to Create/Modify

- `hs-style/src/global_args.rs`: add `is_json()`, `color_str()` methods
- `paper/Cargo.toml`: remove `[[bin]]`
- `paper/src/main.rs`: delete
- `paper/src/lib.rs`: add `pub mod cli/commands/exit_codes/output`
- `paper/src/cli.rs`: remove `Cli`/`GlobalOpts`, rename `NounCmd` -> `PaperCmd`, fix imports
- `paper/src/commands/mod.rs`: add `dispatch()`
- `paper/src/commands/paper.rs`: `GlobalOpts` -> `GlobalArgs`, `paper::` -> `crate::`
- `paper/src/commands/config.rs`: same changes
- `paper/src/output.rs`: `paper::` -> `crate::`
- `paper/src/exit_codes.rs`: `paper::` -> `crate::`
- `Cargo.toml` (workspace): add `crates/*` back to members
- `crates/hs/Cargo.toml`: new
- `crates/hs/src/main.rs`: new
- `crates/hs/src/cli.rs`: new
- `.github/workflows/ci.yaml`: new
- `.github/workflows/release.yaml`: new
- `docs/install.sh`: new
- `docs/install.ps1`: new

## Build Order

1. **hs-style helpers**: Add methods to GlobalArgs (no dependencies, enables everything else)
2. **Paper library refactor**: Make paper library-only, export CLI types and dispatch
3. **hs crate**: Create the binary that wires it all together
4. **Workspace config**: Update members, verify compilation
5. **CI/CD**: Workflows and install scripts
6. **Paper repo cleanup**: Delete old CI/CD from paper

## Steps

### Step 1: Add helper methods to GlobalArgs

**What you'll build:** Convenience methods on `hs_style::global_args::GlobalArgs`
**Key pattern:** Extending a shared type so domain crates don't need wrappers
**Status:** [ ] Not started

**File:** `hs-style/src/global_args.rs`

Add an `impl GlobalArgs` block after the struct with `is_json()` and `color_str()`.

**Verify:** `cargo check -p hs-style`

---

### Step 2: Remove paper's binary and promote modules

**What you'll build:** Convert paper from lib+bin to library-only
**Key pattern:** Extracting a binary's logic into a library for reuse
**Status:** [ ] Not started

**Substeps:**
1. Remove `[[bin]]` section from `paper/Cargo.toml`
2. Delete `paper/src/main.rs`
3. Add `pub mod cli; pub mod commands; pub mod exit_codes; pub mod output;` to `paper/src/lib.rs`

**Verify:** Won't compile yet (import errors) - that's expected, next step fixes them.

---

### Step 3: Refactor paper's cli.rs

**What you'll build:** Clean up CLI types now that they're library exports
**Key pattern:** Separating parser definition from subcommand definitions
**Status:** [ ] Not started

**Substeps:**
1. Delete the `Cli` struct (top-level parser moves to hs)
2. Delete the `GlobalOpts` struct and its `impl` block
3. Rename `NounCmd` to `PaperCmd`
4. Change `paper::models::SearchType` to `crate::models::SearchType` in `From` impls
5. Change `paper::models::SortBy` to `crate::models::SortBy` in `From` impls

---

### Step 4: Refactor paper's command handlers

**What you'll build:** Update handlers to accept `GlobalArgs` directly
**Key pattern:** Removing wrapper types in favor of shared interfaces
**Status:** [ ] Not started

**In `paper/src/commands/paper.rs`:**
1. Replace all `paper::` with `crate::` (these modules are now in the same crate)
2. Replace `use crate::cli::{GlobalOpts, ...}` with `use hs_style::global_args::GlobalArgs`
3. Change function signatures: `global: &GlobalOpts` -> `global: &GlobalArgs`
4. Change `global.quiet()` to `global.quiet`
5. Update `ProviderArg` / `SearchTypeArg` / `SortByArg` imports to use `crate::cli::`

**In `paper/src/commands/config.rs`:**
1. Same `GlobalOpts` -> `GlobalArgs` change
2. `paper::config::Config` -> `crate::config::Config`

**In `paper/src/output.rs`:**
1. `paper::models::` -> `crate::models::`

**In `paper/src/exit_codes.rs`:**
1. `paper::error::PaperError` -> `crate::error::PaperError`

---

### Step 5: Add dispatch function

**What you'll build:** Single entry point for the hs binary to call paper
**Key pattern:** Command dispatch as a library export
**Status:** [ ] Not started

**File:** `paper/src/commands/mod.rs`

Add `pub async fn dispatch(cmd: PaperCmd, global: &GlobalArgs, reporter: &Arc<dyn Reporter>, styles: &Styles) -> Result<()>` that matches on PaperCmd and calls the handlers.

**Verify:** `cargo check -p paper` should pass now.

---

### Step 6: Create the hs crate

**What you'll build:** The unified CLI binary
**Key pattern:** Thin binary that delegates to domain libraries
**Status:** [ ] Not started

**Substeps:**
1. Create `crates/hs/Cargo.toml`
2. Create `crates/hs/src/cli.rs` with `Cli` and `TopCmd` enums
3. Create `crates/hs/src/main.rs` with runtime setup and dispatch
4. Update workspace `Cargo.toml` members to include `crates/*`

**Verify:** `cargo run -p hs -- paper search "test"` and `cargo run -p hs -- --help`

---

### Step 7: CI/CD and install scripts

**What you'll build:** GitHub Actions workflows and curl-installable scripts
**Key pattern:** Cross-platform release pipeline
**Status:** [ ] Not started

**Substeps:**
1. Create `.github/workflows/ci.yaml`
2. Create `.github/workflows/release.yaml`
3. Create `docs/install.sh`
4. Create `docs/install.ps1`

---

### Step 8: Clean up paper repo CI/CD

**What you'll build:** Nothing - removing old workflows
**Status:** [ ] Not started

Delete from paper submodule: `.github/workflows/`, `docs/install.sh`, `docs/install.ps1`

---

## Known Dragons

- **`paper::` vs `crate::` imports**: When binary modules move into the library, all `paper::` references must become `crate::`. Miss one and you get confusing "unresolved import" errors.
- **Exit code double-match**: `cli.command` is moved into the async dispatch block. Capture the exit code mapper function pointer *before* the move.
- **`crates/*` glob**: Workspace member glob `crates/*` requires at least one crate to exist with a Cargo.toml. The hs crate satisfies this.

## Session Log

- 2026-03-20: Plan created from design session
