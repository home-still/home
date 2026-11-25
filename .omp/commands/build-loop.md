---
description: One iteration of the home-still improve-and-release loop
argument-hint: "[optional focus, e.g. 'P0-27' or 'search quality']"
---

# home-still: Most Valuable Change, Released

You are one iteration of a loop on home-still, a self-hosted research
engine: find papers, convert PDFs to markdown, embed, and serve semantic
search through `hs` and MCP. Pick the single most valuable change for its
users, prove it with numbers, release it, log it. Then stop. $ARGUMENTS

"Valuable" means: documents are not silently lost, search finds the
right passage, operators hit fewer sharp edges, and the fleet stays up.

## Directory (every agent uses exactly this)
```
docs/loop/
  log.md   queries.yaml   needs-decision/
  runs/YYYY-MM-DD-NN/
    research.md  plan.md  eval.md  perf.md
    evidence/ (before, after, logs, samples)
```

## 1. Orient
- Read `docs/loop/log.md`, `BACKLOG.md` (P0 first), `git log` since the
  latest release, and open issues.
- Check the live state: `hs status`, `system_status`, `scribe_health`,
  `distill_status`. Note the latest release and what the fleet runs.
- If a previous iteration left work in progress (uncommitted changes,
  untagged release, a half-finished log entry), finish or revert it
  before starting anything new.

## 2. Choose the single most valuable item
Rank by user harm:
1. Data loss or silent drops (documents rejected or stuck, bad data
   indexed as good).
2. Wrong or missing search answers.
3. Reliability and resource problems (VRAM gates, cold starts, crash
   loops).
4. Operator sharp edges, stale docs, missing features.
5. Polish.

Gather evidence first, never guess: failure counts from the catalog and
logs, a fixed query set (`queries.yaml`: 15-20 realistic queries with the
passages they should find) run through `distill_search`,
`distill_scan_repetitions`, `distill_reconcile` and `catalog_repair`
output. One sentence on why your pick beats the runner-up. Carve
oversized items into the most valuable slice.

## 3. Research first
Use home-still on itself before any code: search the corpus for
literature on the problem (OCR quality gating, repetition and
degeneration detection, retrieval, chunking, reranking, evaluation,
queueing and backpressure). Fill gaps with `paper_search` and
`paper_download`. Write `research.md` (sources, technique chosen,
expected effect), then `plan.md` (design, touched crates, tests,
rollout, risk). No research, no build.

## 4. Implement within the project's rules
- One path per feature: no fallbacks, shims, stubs or degraded
  substitutes. Fail loudly with an actionable error.
- Distill runs on CUDA only; never fall back to CPU.
- Destructive operations stay CLI-only, dry-run by default or behind a
  typed confirmation. MCP tools stay report-only.
- Never rebuild, purge or re-embed the corpus, delete durable consumers,
  or touch live services without explicit approval. Test on copies and
  small subsets. Check `hs status` before using the GPU; another agent
  may hold it.
- Decisions that need the operator (re-ingesting dropped documents,
  firewall or sudo steps, changes to a shared or vendored crate) go to
  `needs-decision/` with options and a recommendation. Write them up,
  then pick another item.
- No real IPs, hostnames or secrets in committed docs.

## 5. Verify with numbers
- `cargo fmt --check`, `cargo clippy -D warnings` in all configurations,
  the full test suite, and rustdoc `-D warnings`.
- Before/after in `eval.md`, on the same data: for a quality-gate change,
  measure false-positive and recall rates on a sample of accepted
  documents and on known-bad inputs; for a search change, re-run
  `queries.yaml`. A change must not lower results on cases it did not
  target.
- Run the change end to end on real documents, including ones production
  mishandled. Record timings and resource use in `perf.md`.

If verification fails, fix it or revert. Do not release unverified work.

## 6. Release
- Update README, docs and the changelog; fix any stale claim you touched
  (the README's GLM-OCR "via Ollama" line is a known one).
- Commit, then tag the next `v0.0.1-rc.N` by the project's procedure:
  fmt, clippy and tests green first, version baked in via
  `HS_RELEASE_TAG`, GitHub Actions builds the artifacts. Never copy a
  binary to a host by hand.
- Confirm the release exists and its artifacts built. Do not run
  `hs upgrade` on the fleet; that is the operator's call. Say which hosts
  are behind.

## 7. Log
Append to `docs/loop/log.md`: change, evidence, why it ranked first,
before/after numbers, release tag, commit, the fleet's version gap, and
the top 5 remaining items. Update `BACKLOG.md`: mark what is done, add
what you found.

## 8. Clean up when you are done

Worktrees and build directories outside the clone are not free: a `target/` for this crate is tens of GB, big's
`/tmp` is RAM, and on 2026-10-27 earlier passes had left 37 GB of baseline builds and 5 GB of scratch behind.
**After you ship and before you write the pass's last log line, whether or not you shipped,** remove what *you*
made this pass, and list what earlier passes left.

1. **A baseline is a pair that can be found.** Make it with `git worktree add --detach <path> <sha>` (never a
   branch name: a worktree with a branch checked out stops anything else checking that branch out) and build it
   into `<path>-target`. Then `git worktree list`, plus the `*-target` directory beside each worktree, is the
   whole inventory.
2. **Remove yours.** `git worktree remove <path>` (plain, never `--force`) and `rm -rf <path>-target`, then your
   scratch under `/tmp` (captures, frames, venvs, gate logs, `rustdoctest*`). Remove only what you created this
   pass, and name each path in the log. If git refuses because the worktree has a change in it, name the file
   in the log and leave it.
3. **Leftovers from earlier passes are listed, never deleted.** A killed pass cannot clean up after itself, but
   an agent does not delete what it did not create, so list them once and let a person approve the removal:

   ```sh
   git worktree list --porcelain | awk '/^worktree /{sub(/^worktree /,""); print}' | tail -n +2   # all but the clone
   du -sh <wt> <wt>-target                       # the size of each one that is not yours
   git -C <wt> status --short                    # empty: nothing is lost by removing it
   find <wt> <wt>-target -mmin -120 -print -quit # empty: untouched for two hours, so no live build is using it
   ```

   Log each with its path, size, and whether it is clean, already on `origin/main` and idle (the likeliest to be
   stale). Say so, and still leave it.
4. **Never remove** the clone itself, its own `target/` (the build cache the next pass starts from), anything you
   did not create this pass, or anything a process is using.
5. Write one log line: what you removed and the space it freed (`df -h .` before and after), and the leftovers
   you listed.

## Rules
- One change per iteration: researched, verified with numbers, released,
  logged.
- Every claim of improvement has a number or a reproducible example.
- If blocked, log it and pick the next item.
