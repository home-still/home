# Perf — failed conversions by kind (rc.386)

## `hs status` (which calls `system_status`)

Interleaved, n = 5 each, live catalog (18,030 documents, 2,208 stamps),
each CLI against its own MCP server on big (`evidence/status_latency.txt`):

| | median | min | max |
|---|---|---|---|
| rc.382 `hs` + `hs-mcp` | 8.77 s | 8.24 s | 9.73 s |
| rc.386 `hs` + `hs-mcp` | **8.27 s** | 8.17 s | 8.63 s |

The tally replaces a `filter().count()` over the catalog rows the status
already lists with one pass that counts reasons into a small map (11
distinct reasons today); the time is the catalog listing either way.
The rc.386 server's peak RSS during these runs: 239 MB (VmHWM).

## Payload

`system_status.pipeline` grows from one integer to an object with one
entry per distinct reason (11 today: ~1 KB of JSON). `catalog_list` rows
carry `{reason, kind}` or `null` instead of a boolean.

## `reconvert-failed --dry-run`

Same catalog listing as before; selection is one pass over the stamped
rows. Both runs completed in under a minute; the change in what a real
run does is the work it no longer queues: **155** bad sources that would
fail again (2,208 → 2,053 re-queued).
