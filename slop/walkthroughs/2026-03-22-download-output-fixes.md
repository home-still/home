# Walkthrough: Fix Download Output (Sizing, Errors, Layout)

**Date:** 2026-03-22
**Status:** Planning
**Checkpoint:** c867f748f5ce753df29141fae54b0595894d5fdc

## Goal

Fix the `hs paper download` output so progress bars align, error messages are context-aware, and failures display cleanly without duplication.

## Acceptance Criteria

- [ ] `Completed`/`Failed` events use the same index as `Started` (bar finalization works)
- [ ] Error message says "Set unpaywall_email" only when it's not configured
- [ ] Failed downloads align with successful ones (same prefix width)
- [ ] Failed downloads show in red, no progress bar (just title + short error)
- [ ] No duplicate warning lines after the progress bars
- [ ] Spinner template uses same prefix width as bar template

## Technical Approach

### Architecture

Three crates involved:
- `hs-style` — owns the `Reporter`/`StageHandle` traits and `TtyReporter` (indicatif)
- `paper` services — `download.rs` emits `DownloadEvent` callbacks
- `paper` commands — `paper.rs` wires events to progress bars

### Key Decisions

- **Add `finish_failed` to StageHandle**: Separate method (vs. parameter on `finish_with_message`) because failure styling is fundamentally different — red prefix, no bar
- **40% prefix width formula**: Proportional to terminal width rather than fixed subtraction, gives better balance across terminal sizes
- **Remove warning loop**: Inline failure messages are sufficient; warnings were pure duplication

### Files to Create/Modify

- `paper/src/services/download.rs`: Fix index bug (count → i)
- `paper/src/providers/downloader.rs`: Conditional error message
- `hs-style/src/reporter.rs`: Add `finish_failed` to trait + NoopStageHandle
- `hs-style/src/tty_reporter.rs`: Implement `finish_failed`, fix spinner, fix proportions
- `paper/src/commands/paper.rs`: Use `finish_failed`, remove duplicate warnings

## Build Order

1. **Index bug fix** (`download.rs`): Root cause — everything else looks broken because bars never finalize
2. **Error message** (`downloader.rs`): Standalone, improves the text that gets displayed
3. **Trait extension** (`reporter.rs`): Prerequisite for display changes
4. **TtyReporter** (`tty_reporter.rs`): Layout fixes + `finish_failed` impl
5. **Command wiring** (`paper.rs`): Uses all the above

## Steps

### Step 1: Fix the Index Bug

**What you'll build:** Fix `Completed`/`Failed` events to use enumeration index `i` instead of atomic counter `count`
**File:** `paper/src/services/download.rs`
**Status:** [ ] Not started

The `Started` event uses `index: i` (line 58), but `Completed` (line 73) and `Failed` (line 82) use `index: count` from the atomic counter. Since the HashMap in `paper.rs` keys bars by `i`, the lookup fails and bars never get their finish message.

**Verify:** `cargo check -p paper`

### Step 2: Context-Aware Error Message

**What you'll build:** Make the "no OA PDF" error message conditional on whether `unpaywall_email` is configured
**File:** `paper/src/providers/downloader.rs`
**Status:** [ ] Not started

**Verify:** `cargo check -p paper`

### Step 3: Add `finish_failed` to StageHandle Trait

**What you'll build:** New trait method for rendering failures differently from successes
**Files:** `hs-style/src/reporter.rs`
**Status:** [ ] Not started

**Verify:** `cargo check -p hs-style`

### Step 4: Implement Layout Fixes in TtyReporter

**What you'll build:** `finish_failed` implementation, spinner alignment, prefix width formula
**File:** `hs-style/src/tty_reporter.rs`
**Status:** [ ] Not started

**Verify:** `cargo check -p hs-style`

### Step 5: Wire Up in paper.rs

**What you'll build:** Use `finish_failed` for failures, remove duplicate warnings, truncate inline errors
**File:** `paper/src/commands/paper.rs`
**Status:** [ ] Not started

**Verify:** `cargo check -p paper && hs paper download "autistic female"` (visual check)

---
*Plan created: 2026-03-22*
