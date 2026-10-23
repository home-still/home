# Eval — failed conversions reported by what a retry can do (rc.386)

Same live catalog for both sides (18,030 documents, 2,208
`conversion_failed` stamps), read 2026-10-23. Before: the deployed rc.382
`hs` and `hs-mcp`. After: rc.386 `hs` against an rc.386 `hs-mcp` started on
a spare loopback port (`HS_MCP_URL`), so nothing live was touched. The
dashboard was captured by running the real binary in a pseudo-terminal
and replaying its output through a terminal emulator (`evidence/*_tui_*`).

## 1. What the operator sees

| | rc.382 | rc.386 |
|---|---|---|
| failure row | **"Corrupted PDFs 2208"** (red, no note) | **"Failed, retry 2053"** + `hs pipeline reconvert-failed --dry-run` + "gapped_conversion 1583 · 155 bad source" |
| conversion progress (Markdown %) | **100.0 %** (15,884 / 15,821, capped) | **88.9 %** (15,884 / 17,875) |
| text status | `Corrupted : 2208` | `Failed : 2053 retryable — recover with …`, the four retryable reasons, `Bad source: 155 (…)` |
| `system_status.pipeline` | `corrupted_pdfs: 2208` | `failed_conversions: {retryable 2053, bad_source 155, unclassified 0, reasons[11]}` |
| `catalog_list` row | `corrupted: true` | `conversion_failed: {reason, kind}` |

The live split equals the offline prediction from the catalog copy
(`research.md`): 2,053 retryable + 155 bad source = 2,208, 0 unclassified.

At 110 columns the first version of the row cut its note to
`hs pipeline reconvert-failed` — the live bulk command, without
`--dry-run`. The row now holds the command alone in its detail cell and
the counts in the trailing one; both widths show the whole command
(`evidence/after_tui_110.txt`, and a test pins it).

## 2. What the recovery command selects

| | rc.382 `reconvert-failed --dry-run` | rc.386 |
|---|---|---|
| selected by default | **2,208** (every stamp) | **2,053** (retryable) |
| bad sources re-queued for nothing | 155 | **0** (listed as "Left") |
| `--reason pdf_parse_error` | n/a | 20 selected |
| `--reason gapped` (typo) | n/a | exit 1: "no catalog row is stamped with reason `gapped`; reasons present: …" |

## 3. Cases it did not target

- `Stalled`, `Queued`, `Embedded`, the services table: identical text
  before and after (`evidence/before_status.txt`, `after_status.txt`).
- Search is untouched; RECALL@10 25/25 (`evidence/queries_after.txt`).

## 4. Tests (in the repo)

| test | guards |
|---|---|
| `a_stamps_kind_follows_its_codes_chain_verdict` | every `FailureCode` counted in the kind its chain verdict implies; the untyped escalation is retryable; unknown tokens are unclassified |
| `failed_conversions_count_by_kind_and_list_reasons_largest_first` | the tally and its order |
| `dropped_documents_are_shown_as_retryable_with_the_recovery_command` | the row, its command, top reason and bad-source count; no "Corrupted"; progress 88.9 % |
| `a_narrow_terminal_still_shows_the_whole_dry_run_command` | the 110-column cut above |
| `the_text_status_lists_failures_by_kind_and_reason` | the text lines |
| `by_default_only_retryable_failures_are_requeued`, `a_named_reason_selects_exactly_its_rows_whatever_their_kind`, `a_reason_no_row_carries_is_an_error_naming_the_ones_that_exist` | the recovery command's selection |
