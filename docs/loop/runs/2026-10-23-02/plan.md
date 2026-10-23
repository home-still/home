# Plan — failed conversions are reported by what a retry can do

## Design

One classification, already in the code: `hs_scribe::classify::FailureCode::
class()` — `Permanent` (the document can never convert) or `Escalate` (a
fixed or different backend might). A `conversion_failed` stamp's reason
token maps onto a status kind:

| kind | which stamps |
|---|---|
| retryable | Escalate-class codes, and the chain's untyped escalation token `permanent_convert_failure` |
| bad source | Permanent-class codes |
| unclassified | a token this build does not know (shown, never guessed into a class) |

- `hs-common` (`status.rs`): `FailureKind { Retryable, BadSource,
  Unclassified }` and `FailedConversions { retryable, bad_source,
  unclassified, by_reason }`, with a `tally(reasons, kind_of)` that builds
  it. `PipelineCounts::corrupted_pdfs` is replaced by
  `failed_conversions: Option<FailedConversions>` (clean cutover; a CLI
  reading an older MCP shows the row as unknown, like any other missing
  count).
- `hs-scribe` (`classify.rs`): `stamp_kind(reason) -> FailureKind`, and the
  constant `UNTYPED_ESCALATION` (`"permanent_convert_failure"`) that
  `hs`'s chain stamps — moved here so the token has one definition.
- `hs-mcp`: `system_status` tallies the catalog's stamps with
  `stamp_kind`. `catalog_list`'s `corrupted: bool` becomes
  `conversion_failed: <reason> | null`.
- `hs` status: the "Corrupted PDFs" row becomes **Failed**: the retryable
  count, its note naming the top reason, the bad-source count and
  `hs pipeline reconvert-failed --dry-run`. The text output lists the
  retryable and bad-source reasons. The conversion progress subtracts
  only bad sources.
- `hs pipeline reconvert-failed`: selects **retryable** stamps by default;
  `--reason <token>` (repeatable) selects exactly those reasons, any
  kind. Both the dry run and the run print what is selected and what is
  left, by reason.

## Touched

`hs-common/src/status.rs`, `crates/hs-scribe/src/classify.rs`,
`crates/hs/src/scribe_cmd.rs` (the constant), `crates/hs-mcp/src/main.rs`,
`crates/hs/src/status_cmd.rs`, `crates/hs/src/pipeline_cmd.rs` (+ its CLI
args), READMEs that describe the status or the command.

## Tests

- `stamp_kind`: every `FailureCode` maps to the kind its `class()` implies
  (so a new code cannot silently land in the wrong bucket), the untyped
  escalation token is retryable, an unknown token is unclassified.
- `FailedConversions::tally`: counts per kind and per reason.
- Status rendering: a snapshot with retryable stamps shows the Failed row,
  its top reason and the recovery command; no "Corrupted" text remains;
  progress excludes only bad sources.
- `reconvert-failed` selection: default = retryable only; `--reason`
  selects exactly the named reasons, including a bad-source one.

## Rollout

rc.386; `hs status` needs both the CLI and the MCP server on rc.386 (an
older MCP leaves the row unknown). The recovery itself stays the
operator's decision (BACKLOG P0-28) and should follow the rc.385 deploy
on bmb.

## Risk

- Changes the default selection of `reconvert-failed`: an operator who
  wanted to retry bad sources now names them with `--reason`. That is the
  point; the dry run shows both groups.
- `catalog_list`'s field rename changes an MCP tool's output shape;
  `corrupted` had no consumer in the repo.
