# Research — 1,583 dropped documents were reported as "Corrupted PDFs"

**Date:** 2026-10-23
**Question:** why did a week of permanently dropped documents go
unnoticed, and what is the smallest change that makes the next one
visible and recoverable?

---

## 1. The symptom, measured

`hs status` (and the MCP `system_status` it renders) shows a row
**"Corrupted PDFs: 2,208"**. An operator's status check on 2026-10-23 read
it as a static count of bad files ("Corrupted PDFs 2,208" in its queue
table) and did not flag it.

What the row actually counts: every catalog row stamped
`conversion_failed`, whatever the reason (`hs-mcp/src/main.rs:3535`); its
doc comment says "the source bytes are not a valid PDF" (`hs-common/src/
status.rs:131`). The 2,208 stamps, by reason (read-only copy of the
catalog, 18,007 rows):

| reason | rows | class (`classify.rs`) | stamped |
|---|---|---|---|
| `gapped_conversion` | **1,583** | Escalate | 59, 187, 109, 357, **472**, **397** per day (10-17 … 10-23) |
| `vlm_transport_error` | **432** | Escalate | **423 on 2026-10-17** (one incident), 9 since |
| `empty_conversion` | 64 | Permanent | |
| `paywall_html` | 33 | Permanent | |
| `unsupported_content_type:binary` / `:html` | 23 / 1 | Permanent | |
| `vlm_repetition_loop` | 21 | Escalate | |
| `pdf_parse_error` | 20 | Permanent | |
| `permanent_convert_failure` | 17 | the chain's untyped escalation | |
| `source_missing` | 13 | Permanent | |
| `html_not_utf8` | 1 | Permanent | |

So **2,053 (93 %)** of the "corrupted PDFs" are documents a backend
failed on, which a fixed or different backend can convert. Only **155**
are bad sources. The gapped ones are the live data loss rc.385 fixes;
they were climbing by ~400 a day under a label that says "your input is
broken".

Two more effects of the same label:

- The TUI subtracts the count from the conversion progress denominator
  (`status_cmd.rs:527`), so every dropped document made progress look
  **better**.
- The recovery command, `hs pipeline reconvert-failed`, re-queues every
  stamp, bad sources included, so it cannot be pointed at "what a fix
  recovers".

## 2. What the literature says

- **Hohpe & Woolf, *Enterprise Integration Patterns*** (in the corpus)
  separate an **Invalid Message Channel** — messages that "can be
  delivered and received but not processed" (bad content) — from a
  **Dead Letter Channel**, messages "the messaging system cannot deliver
  properly". They are different channels because they need different
  responses, and the invalid-message channel is monitored: the system
  "alerts system administrators whenever the channel contains messages".
- **Avižienis, Laprie, Randell & Landwehr, *Basic Concepts and Taxonomy
  of Dependable and Secure Computing*** (in the corpus, p. 14): failure
  modes are characterized along four viewpoints, one of which is
  **detectability**, "the signaling of service failures to the user(s)";
  a *silent* failure delivers no signal at all. A failure counted under
  the wrong name is signaled to nobody who can act on it.
- The **retry tactic** (software-architecture tactics, in the corpus)
  "assumes that the fault that caused a failure is transient, and that
  retrying the operation may lead to success" — retry is only correct for
  the class of failure where it can work.

The project already has the classification these sources ask for:
`hs_scribe::classify::FailureCode::class()` decides Permanent (the
document can never convert) vs Escalate (another backend might). The
status and the recovery command just do not use it.

## 3. The change

1. **Status reports failed conversions by class**, not as "Corrupted
   PDFs": *retryable* (Escalate-class reasons, and the chain's untyped
   escalation token), *bad source* (Permanent-class), and *unclassified*
   (a reason this build does not know — shown, never guessed), each with
   its largest reasons, and the retryable row names the recovery command.
2. **Progress subtracts only bad sources**: a document a backend dropped
   is unfinished work, not an exclusion.
3. **`hs pipeline reconvert-failed` selects retryable stamps by
   default**, and `--reason <token>` selects exact reasons (any class) —
   for after a fix to a parser, say. The dry run shows what is selected
   and what is left.
4. `catalog_list`'s per-row `corrupted: bool` (same mislabel) becomes the
   stamp's reason.

## 4. Expected effect

On today's catalog: the status reads **2,053 retryable** (with
`gapped_conversion 1,583` at the top) and **155 bad source**, instead of
"Corrupted PDFs 2,208"; conversion progress drops from
15,884/15,821 (capped at 100 %) to 15,884/17,874 = 88.9 %, the real
figure; and the recovery command, once rc.385 is on bmb, re-queues the
2,053 documents that can come back and leaves the 155 that cannot.
