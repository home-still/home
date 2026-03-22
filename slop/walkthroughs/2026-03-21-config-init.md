# Walkthrough: `config init` — Generate Default Config YAML

**Date:** 2026-03-21
**Status:** Planning
**Checkpoint:** a21b9623de87d443b8fb7fcdde544147fcf3efae

## Goal

Add a `config init` subcommand that creates `~/.home-still/config.yaml` with documented defaults, so users have a real file to edit instead of relying on invisible in-code defaults.

## Acceptance Criteria

- [ ] `hs paper config init` creates `~/.home-still/config.yaml` with all current defaults
- [ ] The generated YAML includes comments explaining each section
- [ ] Running `init` when file already exists prints a warning and does NOT overwrite (unless `--force`)
- [ ] `hs paper config show` output matches the generated file's structure
- [ ] `hs paper config init --force` overwrites an existing file

## Technical Approach

### Architecture

The config system already has a layered Figment loader (`Config::load()`) that checks:
1. In-code `Default` impl
2. `/etc/home-still/config.yaml`
3. `~/.home-still/config.yaml`
4. `~/.home-still/paper/config.yaml`
5. `HOME_STILL_*` env vars

What's missing: a way to **write** the defaults to disk. Since `Config` derives `Serialize`, we can serialize defaults to YAML. But `serde_yaml_ng::to_string` produces bare YAML without comments — we need a template approach for nice output.

### Key Decisions

- **Template string, not serde serialization**: A handwritten YAML template with comments is more user-friendly than machine-generated YAML. We embed the template as a `const &str` in the config module and interpolate default values. This way users get documented, readable config out of the box.
- **Single file at `~/.home-still/config.yaml`**: Generate the user-wide config, not the app-specific one. This is the natural "first config" users should edit. The app-specific path (`~/.home-still/paper/config.yaml`) is for power users who want per-tool overrides.
- **`--force` flag, not interactive prompt**: CLI tools should be scriptable. A `--force` flag is cleaner than "overwrite? y/n".

### Dependencies

- No new external crates needed
- Uses: `std::fs` (create_dir_all, write), `dirs` (home_dir)

### Files to Create/Modify

- `paper/src/config.rs`: Add `Config::generate_default_yaml()` method that returns the commented template string
- `paper/src/cli.rs`: Add `Init` variant to `ConfigAction` with `--force` flag
- `paper/src/commands/config.rs`: Add `ConfigAction::Init` handler — check existence, create dirs, write file

## Build Order

1. **`config.rs` — template method**: Pure function, no side effects, easy to verify. Returns the YAML string with comments and defaults baked in.
2. **`cli.rs` — `Init` variant**: Add the CLI plumbing so clap knows about the new subcommand.
3. **`commands/config.rs` — handler**: Wire it up — check if file exists, create parent dirs, write the template, report success/skip.

## Anticipated Challenges

- **YAML comment formatting**: YAML comments are just `# ...` lines. Since we're using a template string (not serde), we have full control. No dragon here.
- **Path creation**: `~/.home-still/` may not exist. `std::fs::create_dir_all` handles this idempotently.
- **Windows paths**: `dirs::home_dir()` handles cross-platform. The template will use forward slashes in example paths but the actual `download_path` default uses the platform path separator.

## Steps

### Step 1: Add `Config::generate_default_yaml()` to `paper/src/config.rs`

**What you'll build:** A method that returns a `String` containing the full default config as commented YAML.

**Key pattern:** Embed the template as a Rust string literal with `include_str!` or inline `format!`. Since defaults are known at compile time, a simple `const` or function returning a string works.

**Files:** `paper/src/config.rs`

### Step 2: Add `Init` variant to `ConfigAction` in `paper/src/cli.rs`

**What you'll build:** The CLI argument definition for `hs paper config init [--force]`.

**Key pattern:** `clap` derive — add a new variant to the existing `ConfigAction` enum.

**Files:** `paper/src/cli.rs`

### Step 3: Implement the handler in `paper/src/commands/config.rs`

**What you'll build:** The logic that checks for existing file, creates directories, writes the YAML, and reports what happened.

**Key pattern:** Guard clause pattern — check for existing file first, bail or warn, then do the write.

**Files:** `paper/src/commands/config.rs`

---
*Plan created: 2026-03-21*
*User implementation started: [to be updated]*
