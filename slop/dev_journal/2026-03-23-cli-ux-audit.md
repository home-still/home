# Dev Journal: 2026-03-23 - CLI UX Audit & Fixes

**Session Duration:** ~1 hour
**Walkthrough:** `slop/walkthroughs/2026-03-23-cli-ux-improvements.md`

## What We Did

Audited the `hs` CLI against research docs in `slop/research/` (CLI design guide, CLI UX guide, metasearch UX design). Identified 11 gaps, implemented 8 fixes across 11 files.

### Changes Implemented

1. **Global flags grouped in help** -- Added `help_heading = "Global Options"` to all `GlobalArgs` fields so `--help` separates command-specific flags from global flags.

2. **FORCE_COLOR support** -- Added `FORCE_COLOR` env var check in `mode::detect()`. Also fixed a bug where line 29 checked `NO_COLOR` instead of `TERM` for dumb terminal detection.

3. **Config path constant** -- Extracted `CONFIG_REL_PATH` constant to `hs-style/src/lib.rs`, replaced hardcoded `".home-still/config.yaml"` in `paper/src/config.rs` and `crates/hs/src/main.rs`. Config errors now mention `hs config init`.

4. **Help text examples** -- Added `after_help` with formatted examples at all three levels: `hs --help`, `hs paper --help`, and each subcommand. Replaced doc comment examples that clap was mashing onto one line.

5. **Error remediation hints** -- Updated `PaperError` variants with actionable suggestions (e.g., "Try --provider" for unavailable, "Try: hs paper search" for not found).

6. **`--yes`/`-y` flag** -- Added to `GlobalArgs` for non-interactive use. `hs config init -y` skips confirmation prompt.

7. **Empty results guidance** -- Search and download now give contextual hints when zero results (e.g., "Try --provider all", "Try broadening your query").

8. **Pipe-mode output** -- Added `print_search_result_pipe()` for tab-separated one-line-per-result format. Threaded `OutputMode` through `dispatch()` -> `run_search()` to select format.

### Skipped

- **`--dry-run` for downloads** -- Redundant with existing `hs paper search` as a preview mechanism.
- **Per-source search progress** -- Large change, deferred.
- **pdf-mash hs-style integration** -- Separate project.

## Bugs & Challenges

### TERM=dumb detection was checking NO_COLOR

**Symptom:** `hs-style/src/mode.rs` line 29 checked `NO_COLOR` env var for value `"dumb"` instead of `TERM`.

**Root Cause:** Copy-paste error -- the `NO_COLOR` check on line 25 was duplicated instead of changing the var name to `TERM`.

**Solution:** Changed `std::env::var("NO_COLOR")` to `std::env::var("TERM")` on that line.

**Lesson:** When adding sequential env var checks, verify each one references the correct variable.

### Provider variable shadowed in run_search

**Symptom:** Wanted to check `matches!(provider, ProviderArg::All)` in empty-results hint, but `provider` was shadowed by `make_provider()` on line 46.

**Root Cause:** `let provider = make_provider(&provider, &config)?;` shadows the `ProviderArg` parameter with a `Box<dyn PaperProvider>`.

**Solution:** Used a generic hint ("Try broadening your query or removing filters") instead of the specific `--provider all` suggestion in `run_search`. The download path still has access to the original `ProviderArg` and uses the specific hint.

**Lesson:** Watch for variable shadowing when you need the original value later.

### cli_global vs global naming

**Symptom:** `cargo check` failed with `cannot find value cli_global`.

**Root Cause:** Used `cli_global.yes` but the parameter is named `global` in `handle_config()`.

**Solution:** Changed to `global.yes`.

**Lesson:** Check parameter names in the function signature before referencing them.

### Missing import for OutputMode

**Symptom:** `cargo check` failed with `cannot find type OutputMode in this scope`.

**Root Cause:** Added `OutputMode` parameter to `dispatch()` but forgot the `use hs_style::mode::OutputMode;` import.

**Solution:** Added the import to `paper/src/commands/mod.rs`.

## Code Changes Summary

- `hs-style/src/lib.rs`: Added `CONFIG_REL_PATH` constant
- `hs-style/src/global_args.rs`: Added `help_heading` to all fields, added `--yes`/`-y` flag
- `hs-style/src/mode.rs`: Added `FORCE_COLOR` check, fixed `TERM=dumb` bug
- `crates/hs/src/cli.rs`: Added `after_help` examples to `Cli` and `Paper`
- `crates/hs/src/main.rs`: Used `CONFIG_REL_PATH`, pass `mode` to dispatch, `--yes` skips confirm
- `paper/src/cli.rs`: Switched examples from doc comments to `after_help`
- `paper/src/config.rs`: Used `CONFIG_REL_PATH`, improved error context
- `paper/src/error.rs`: Added remediation hints to error messages
- `paper/src/commands/mod.rs`: Added `OutputMode` param to `dispatch()`
- `paper/src/commands/paper.rs`: Empty results hints, pipe-mode dispatch, `mode` param
- `paper/src/output.rs`: Added `print_search_result_pipe()` for tab-separated pipe output

## Patterns Learned

- **`help_heading`**: clap attribute that groups flags under a labeled section in `--help`
- **`after_help`**: clap attribute for content after the flags list -- better than doc comments for multi-line examples
- **Threading state through dispatch**: When a new parameter is needed deep in command handlers, it must be added to every function in the call chain (dispatch -> handler)

## Next Session

- Commit all changes
- Consider per-source search progress (larger effort)
- pdf-mash hs-style integration (separate walkthrough)
