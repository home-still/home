# Walkthrough: Rename paper-fetch to paper

**Date:** 2026-03-08
**Status:** Planning
**Checkpoint:** ea5ec4aa1f7eaa92ff3b6235e16e24432b9604bb
**Walkthrough:** 1 of 3

## Goal

Rename the entire project from `paper-fetch` to `paper` — directories, crate names, binary name, imports, error types, config paths, and CLI help text.

## Acceptance Criteria

- [ ] `crates/paper-fetch/` renamed to `crates/paper/`
- [ ] `crates/paper-fetch-core/` renamed to `crates/paper-core/`
- [ ] Binary is named `paper` (not `paper-fetch`)
- [ ] All `paper_fetch_core` imports compile as `paper_core`
- [ ] `PaperFetchError` renamed to `PaperError` everywhere
- [ ] Config paths use `.home-still/paper/` (not `paper-fetch/`)
- [ ] CLI help text shows `paper` in all examples
- [ ] `.gitmodules` exists with HTTPS URL for hs-style
- [ ] `cargo build && cargo test --workspace` passes
- [ ] `./target/debug/paper paper search "test"` runs

## Technical Approach

### Architecture

This is a mechanical rename with no logic changes. The workspace uses `crates/*` glob, so after renaming directories, the workspace auto-discovers the new paths. The tricky part is getting every string occurrence — miss one and the build breaks.

### Key Decisions

- **Error type name**: `PaperFetchError` → `PaperError` (shorter, matches new project name)
- **CLI crate package name**: `paper-cli` (distinguishes from a hypothetical `paper` library crate)
- **Core crate package name**: `paper-core`

### Dependencies

No new dependencies. This is pure renaming.

### Files to Create/Modify

See Steps below for exact list.

## Build Order

1. **`.gitmodules`**: Fix the missing submodule config (blocker for CI later)
2. **Directory renames**: `git mv` the crate directories
3. **Cargo.toml files**: Update package names, binary name, dependency references
4. **Core crate imports**: `PaperFetchError` → `PaperError` across all core source files
5. **CLI crate imports**: `paper_fetch_core` → `paper_core` across all CLI source files
6. **Config paths**: `.home-still/paper-fetch/` → `.home-still/paper/`
7. **CLI help text**: `paper-fetch` → `paper` in doc comments and command name
8. **Build + test**: Verify everything compiles

## Anticipated Challenges

- **Missing an occurrence**: The compiler will catch any missed `paper_fetch_core` imports, but string literals in help text and config paths won't cause compile errors if missed — review carefully.
- **Cargo.lock**: Will regenerate automatically when you run `cargo build`, don't edit it manually.

---

## Steps

### Step 1: Create `.gitmodules`

**What you'll build:** The missing git submodule configuration file
**Why first:** CI workflows need this to checkout the hs-style submodule

Create a new file `.gitmodules` in the repo root:

```ini
[submodule "hs-style"]
    path = hs-style
    url  = https://github.com/home-still/hs-style.git
```

**Verify:**
```bash
git submodule sync
git submodule update --init --recursive
```

Should complete without errors.

---

### Step 2: Rename crate directories

**What you'll build:** Move the crate directories to their new names

```bash
git mv crates/paper-fetch crates/paper
git mv crates/paper-fetch-core crates/paper-core
```

**Why `git mv`:** Preserves git history for the files.

**Verify:** `ls crates/` should show `paper/` and `paper-core/` (no `paper-fetch*`).

---

### Step 3: Update Cargo.toml files

**What you'll change:** Package names, binary name, and dependency paths

#### File: `crates/paper-core/Cargo.toml`

Change line 2:
```toml
# OLD
name = "paper-fetch-core"

# NEW
name = "paper-core"
```

#### File: `crates/paper/Cargo.toml`

Three changes:
```toml
# OLD
name = "paper-fetch-cli"
...
name = "paper-fetch"
...
paper-fetch-core = { path = "../paper-fetch-core" }

# NEW
name = "paper-cli"
...
name = "paper"
...
paper-core = { path = "../paper-core" }
```

#### File: `Cargo.toml` (workspace root)

No changes needed — `members = ["crates/*", "hs-style"]` auto-discovers by directory.

**Verify:** Don't build yet — imports will break until Step 4.

---

### Step 4: Rename `PaperFetchError` → `PaperError` in core crate

**What you'll change:** The error enum name across all core source files
**Key pattern:** Find-and-replace `PaperFetchError` with `PaperError` in every file under `crates/paper-core/src/`

#### Files to update (10 files):

1. **`crates/paper-core/src/error.rs`**
   - Line 4: `pub enum PaperFetchError` → `pub enum PaperError`
   - Line 44: `impl PaperFetchError` → `impl PaperError`
   - Line 60: `PaperFetchError::RateLimited` → `PaperError::RateLimited`

2. **`crates/paper-core/src/providers/arxiv.rs`**
   - Replace all `PaperFetchError` with `PaperError` (8+ occurrences)

3. **`crates/paper-core/src/providers/downloader.rs`**
   - Replace all `PaperFetchError` with `PaperError` (5 occurrences)

4. **`crates/paper-core/src/resilience/rate_limiter.rs`**
   - Replace all `PaperFetchError` (2 occurrences)

5. **`crates/paper-core/src/resilience/retry.rs`**
   - Replace all `PaperFetchError` (3 occurrences)

6. **`crates/paper-core/src/ports/provider.rs`**
   - Replace all `PaperFetchError` (4 occurrences)

7. **`crates/paper-core/src/ports/search_service.rs`**
   - Replace all `PaperFetchError` (2 occurrences)

8. **`crates/paper-core/src/ports/download_service.rs`**
   - Replace all `PaperFetchError` (2 occurrences)

9. **`crates/paper-core/src/services/download.rs`**
   - Replace all `PaperFetchError` (2 occurrences)

**Tip:** You can do this with a single command to find all occurrences first:
```bash
grep -rn "PaperFetchError" crates/paper-core/src/
```

Then use your editor's project-wide find-and-replace within `crates/paper-core/src/`.

**Verify:** Don't build yet — CLI crate imports still reference old names.

---

### Step 5: Update CLI crate imports (`paper_fetch_core` → `paper_core`)

**What you'll change:** All `use paper_fetch_core::` imports in the CLI crate, plus the error type references

#### Files to update (5 files):

1. **`crates/paper/src/commands/paper.rs`** — 7 import lines
   ```rust
   // OLD
   use paper_fetch_core::config::Config;
   use paper_fetch_core::models::SearchQuery;
   // etc.

   // NEW
   use paper_core::config::Config;
   use paper_core::models::SearchQuery;
   // etc.
   ```
   Also update line 101: `paper_fetch_core::ports::download_service::DownloadService` → `paper_core::ports::download_service::DownloadService`

2. **`crates/paper/src/commands/config.rs`**
   ```rust
   // OLD
   use paper_fetch_core::config::Config;
   // NEW
   use paper_core::config::Config;
   ```

3. **`crates/paper/src/exit_codes.rs`** — both the import path AND the error type name:
   ```rust
   // OLD
   if let Some(pfe) = cause.downcast_ref::<paper_fetch_core::error::PaperFetchError>() {
       use paper_fetch_core::error::PaperFetchError::*;

   // NEW
   if let Some(pfe) = cause.downcast_ref::<paper_core::error::PaperError>() {
       use paper_core::error::PaperError::*;
   ```

4. **`crates/paper/src/output.rs`**
   ```rust
   // OLD
   use paper_fetch_core::models::{Paper, SearchResult};
   // NEW
   use paper_core::models::{Paper, SearchResult};
   ```

5. **`crates/paper/src/cli.rs`** — line 157:
   ```rust
   // OLD
   impl From<SearchTypeArg> for paper_fetch_core::models::SearchType {
   // NEW
   impl From<SearchTypeArg> for paper_core::models::SearchType {
   ```

**Verify:** `cargo build` — should compile now (but CLI help text still says `paper-fetch`).

---

### Step 6: Update config paths

**What you'll change:** Hardcoded `.home-still/paper-fetch/` paths in the config module

#### File: `crates/paper-core/src/config.rs`

Three replacements:
```rust
// Line 37 (cache_path default)
// OLD
.map(|h| h.join(".home-still/paper-fetch/cache"))
// NEW
.map(|h| h.join(".home-still/paper/cache"))

// Line 47 (config_path method)
// OLD
dirs::home_dir().map(|h| h.join(".home-still/paper-fetch/config.yaml"))
// NEW
dirs::home_dir().map(|h| h.join(".home-still/paper/config.yaml"))

// Line 64 (load method)
// OLD
let app_path = home.join(".home-still/paper-fetch/config.yaml");
// NEW
let app_path = home.join(".home-still/paper/config.yaml");
```

**Verify:** `cargo build` still compiles.

---

### Step 7: Update CLI help text and command name

**What you'll change:** Doc comments and the `#[command(name = ...)]` attribute

#### File: `crates/paper/src/cli.rs`

Replace every `paper-fetch` with `paper` in:
- Line 3: `/// paper-fetch — meta-search tool...` → `/// paper — meta-search tool...`
- Lines 6-10: All example commands (`paper-fetch paper search` → `paper paper search`, etc.)
- Line 12: `#[command(name = "paper-fetch"...)]` → `#[command(name = "paper"...)]`
- Lines 67-68: More examples
- Lines 92, 105-106: Download examples

**Verify:**
```bash
cargo build
./target/debug/paper --help
```

Should show `paper` everywhere, not `paper-fetch`.

---

### Step 8: Update README

#### File: `README.md`

Update the title and any `paper-fetch` references to `paper`.

---

### Step 9: Build, test, and verify

```bash
cargo build
cargo test --workspace
./target/debug/paper --version
./target/debug/paper --help
./target/debug/paper paper search "transformers"
```

All should work with the new `paper` binary name.

---

### Step 10: Commit

```bash
git add -A
git commit -m "rename: paper-fetch → paper (crates, binary, imports, config paths)"
git push
```

---

## Known Dragons

- **Cargo.lock regeneration**: After renaming packages, `cargo build` regenerates `Cargo.lock`. Don't manually edit it.
- **Existing config files**: If you have `~/.home-still/paper-fetch/config.yaml` locally, the tool won't find it after the rename. Copy it to `~/.home-still/paper/config.yaml`.
- **Shell aliases/PATH**: If you had `paper-fetch` in any aliases or scripts, update them to `paper`.

---

*Plan created: 2026-03-08*
*Implementation proven: [to be updated]*
*User implementation started: [to be updated]*
