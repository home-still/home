# Plan — catch-up never re-queues a failed conversion

## Design

- `hs-common/src/status.rs`: `count_unconverted_stems` takes the set of
  stems stamped `conversion_failed` and returns `UnconvertedStems { all,
  unstamped }`. `all` keeps feeding `pipeline_drift` (stamped failures
  stay visible there, as documented); `unstamped` feeds Stalled via
  `record_queue`.
- `crates/hs-mcp` `system_status`: builds the stamped set from the
  catalog listing it already holds.
- `crates/hs/src/pipeline_cmd.rs` `plan_catch_up`: takes the stamped
  stems (reason per stem), drops them from the republish list and
  returns them grouped by `stamp_kind`. `cmd_catch_up` lists the catalog
  (the same parallel fetch `system_status` uses), and reports
  "skipped N stamped failures (R retryable, B bad source) — recover with
  `hs pipeline reconvert-failed --dry-run`".
- The status Stalled note keeps pointing at catch-up; it is now true.

One path: re-queuing a failure is `reconvert-failed`'s job (by kind,
clears the stamp first); catch-up queues documents nothing has tried.

## Touched

`hs-common/src/status.rs`, `crates/hs-mcp/src/main.rs`,
`crates/hs/src/pipeline_cmd.rs`, `crates/hs/README.md` (catch-up line).

## Tests

- `plan_catch_up`: a stamped stem without markdown is not republished and
  is reported with its kind; an unstamped one is.
- `count_unconverted_stems`: `all` includes stamped stems, `unstamped`
  does not.
- Existing catch-up/unconverted tests updated for the new signatures.

## Verification

Before/after on the live stores, dry runs only: `hs pipeline catch-up
--dry-run` (old vs new republish counts, the skipped split), and the
Stalled number from `system_status` (old vs new). No events published.

## Rollout / risk

rc.387. A catalog listing failure now fails catch-up (it cannot tell
failures from new documents without it) — loud, as it should be. A
document stamped by mistake is no longer swept up by catch-up; it shows
in the Failed row and `reconvert-failed --reason` re-queues it.
