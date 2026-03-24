# Walkthrough: CLI UX Improvements

**Date:** 2026-03-23
**Status:** In Progress
**Checkpoint:** 943db4a4fc73722b4c3653ce540aa5d20bdcc817

## Goal

Improve `hs` CLI UX based on research audit -- better help text, error messages, pipe behavior, and missing flags.

## Acceptance Criteria

- [ ] Global flags grouped under separate heading in --help
- [ ] FORCE_COLOR env var supported
- [ ] Config errors mention file path and `hs config init`
- [ ] Help text has properly formatted examples at all levels
- [ ] Error messages include remediation hints
- [ ] `--yes`/`-y` flag for non-interactive use
- [ ] `--dry-run` flag for downloads
- [ ] Empty results give contextual suggestions
- [ ] Pipe-mode outputs parseable one-line-per-result format

## Steps

### Step 1: Group Global Flags in Help

**What you'll build:** Add `help_heading` to GlobalArgs so --help separates global from command flags
**Key pattern:** clap's `#[arg(help_heading = "...")]`
**Status:** [ ] Not started

**File:** `hs-style/src/global_args.rs`

**Verify:** `cargo run -p hs -- paper search --help` shows two flag groups

---

### Step 2: FORCE_COLOR Support

**What you'll build:** Check `FORCE_COLOR` env var in output mode detection
**Key pattern:** Env var precedence chain
**Status:** [ ] Not started

**File:** `hs-style/src/mode.rs`

**Verify:** `FORCE_COLOR=1 cargo run -p hs -- paper search --help | cat` shows colored output

---

### Step 3: Config Path in Error Messages

**What you'll build:** Improve config error context to mention file location and `hs config init`
**Key pattern:** anyhow `.context()` with actionable messages
**Status:** [ ] Not started

**File:** `paper/src/commands/paper.rs` (lines 45, 102, 143)

**Verify:** Run `hs paper search "test"` with missing config -- error should mention path and init command

---

### Step 4: Help Text Formatting

**What you'll build:** Properly formatted examples at all help levels
**Key pattern:** clap `after_help` / `verbatim_doc_comment`
**Status:** [ ] Not started

**Files:** `crates/hs/src/cli.rs`, `paper/src/cli.rs`

**Verify:** `hs --help`, `hs paper --help`, `hs paper search --help` all show formatted examples

---

### Step 5: Error Remediation Hints

**What you'll build:** Three-part error messages (what/why/fix) for all PaperError variants
**Key pattern:** thiserror display with suggestions
**Status:** [ ] Not started

**Files:** `paper/src/error.rs`, `paper/src/commands/paper.rs`

**Verify:** Trigger each error type and verify helpful message appears

---

### Step 6: --yes/-y Flag

**What you'll build:** Global flag to skip interactive prompts
**Key pattern:** Non-interactive CLI for scripts/CI
**Status:** [ ] Not started

**Files:** `hs-style/src/global_args.rs`, `crates/hs/src/main.rs`

**Verify:** `hs config init --yes` skips confirmation prompt

---

### Step 7: --dry-run for Downloads

**What you'll build:** Preview what would be downloaded without downloading
**Key pattern:** Agent-friendly mutation preview
**Status:** [ ] Not started

**Files:** `paper/src/cli.rs`, `paper/src/commands/paper.rs`

**Verify:** `hs paper download "test" --dry-run` shows papers but doesn't download

---

### Step 8: Empty Results Guidance

**What you'll build:** Contextual suggestions when search returns zero results
**Key pattern:** Progressive query relaxation hints
**Status:** [ ] Not started

**Files:** `paper/src/commands/paper.rs`, `paper/src/output.rs`

**Verify:** `hs paper search "xyznonexistent123"` shows helpful suggestions

---

### Step 9: Pipe-Mode Output Format

**What you'll build:** One-line-per-result format when output is piped
**Key pattern:** TTY-adaptive dual-mode output
**Status:** [ ] Not started

**Files:** `paper/src/output.rs`, `paper/src/commands/paper.rs`

**Verify:** `hs paper search "test" | head -5` shows clean, parseable lines

---

## Session Log

- 2026-03-23: Walkthrough created from CLI UX audit
