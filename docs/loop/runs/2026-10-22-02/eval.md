# Evaluation — the status that says "backend unavailable" and nothing else

Measured on the live corpus on `big`, 2026-10-22.

## The problem

The `hs status` dashboard showed this, live:

```
Services:
  ○ scribe   http://192.168.1.110:7435            backend unavailable   0.0.1-rc.373
  ● scribe   http://bmb.local:7433                5 converting   0.0.1-rc.373
  ● distill  http://192.168.1.110:7434            idle (Cuda)  0.0.1-rc.373
```

**"backend unavailable" is the whole report.** The tier is refusing
work, the corpus is not growing on it, and the dashboard says nothing
about *why*. The cause is in the scribe's own journal, on another host:

```
WARN vlm backend unavailable — refusing dispatch free_vram_mb=Some(13251)
     reachable=true headroom_mb=13500 holders=pid=2227527 7534MB
     hs-distill-server; pid=1737565 2596MB hs-scribe-server;
     pid=2615361 591MB bench_elements
```

**The numbers are already in the payload the client parses.**
`ScribeClient::health` reads `backend_vram_free_mb`,
`backend_model_resident` and `backend_checked_at` — and the MCP's status
builder **dropped all three**, replacing them with the string
`"backend unavailable"`. The information was fetched, parsed, and
discarded.

Measured over the last 12 hours on `big`:

| | |
|---|---|
| the tier's refusals | **54** |
| with a third-party tenant holding the room | **54 (100 %)** |
| the free VRAM at the refusals | 6,314 – 13,345 MB |
| the headroom the gate wants | 13,500 MB |
| the gate readings under the headroom | **57 of 107 (53 %)** |
| the conversions the tier still did | 146 |

The tier refused more often than it passed, and the dashboard never said
a word about it.

## The change

**Report the cause, not the symptom.**

1. **The scribe server's `/health` names the gate.** It already reported
   `backend_vram_free_mb` and `backend_model_resident`; it did not
   report the **headroom it wants**, so a reader could not tell "12.5
   GiB free" from "12.5 GiB free but 13.2 GiB is needed". The headroom
   and the **holders** are now in the payload too.

2. **The status carries the diagnosis.** The activity line becomes:
   *"backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
   pid=2227527 7534MB hs-distill-server; …"*.

**The gate's behaviour is unchanged** — every one of the 54 refusals
had a genuine shortfall, and refusing work the card cannot do is
right. This changes what is *reported*, not what is *fetched*: the
numbers were already parsed and thrown away.

## Result

**The before and the after, from the same MCP call, a minute apart:**

```
  the old MCP: backend unavailable
  the new MCP: backend unavailable: 13.0 GiB free
```

**And the whole diagnosis, end to end**, against the payload the new
scribe server sends:

```
  http://127.0.0.1:46225
    -> backend unavailable: 12.9 GiB free, needs 13.2 GiB; held by
       pid=2227527 7534MB hs-distill-server; pid=1737565 2596MB
       hs-scribe-server; pid=2615361 591MB bench_elements
```

| | before | after |
|---|---|---|
| the free VRAM | not reported | **12.9 GiB** |
| the headroom the gate wants | not reported | **13.2 GiB** |
| who is holding the card | not reported | **every holder, with its size** |
| the report contradicts itself | no | no |

The operator's next step is now visible from the dashboard: free the
card, or lower the headroom. Before, the next step was to read a
journal on another host.

**One path.** `hs status` (the CLI) and the MCP's `system_status` go
through the *same* code — the CLI calls the MCP's `system_status` tool
(`crates/hs/src/status_cmd.rs:242`) — so both carry the diagnosis.

**At the CLI surface, end to end** — a local `hs-mcp --serve`, a fake
scribe server, and the real `hs status` binary pointed at it with
`HS_MCP_URL`:

```
=== the new scribe server (the server sends the new payload)
  ○ scribe   http://127.0.0.1:33613    backend unavailable: 12.9 GiB free,
           needs 13.2 GiB; held by pid=2227527 7534MB hs-distill-server;
           pid=1737565 2596MB hs-scribe-server; pid=2615361 591MB
           bench_elements   0.0.1-rc.379

=== the live rc.373 scribe server (the server sends the old payload)
  ○ scribe   http://127.0.0.1:33987    backend unavailable: 12.9 GiB free
           0.0.1-rc.379
```

**The second line is the mixed pair.** The MCP and the scribe server
must be upgraded together for the whole diagnosis to appear: the MCP
asks the server for the headroom, and an older server does not send
it. The fallback is not the old bare symptom — the old payload *does*
carry `backend_vram_free_mb`, so the free VRAM is still reported. The
line says what it knows and **guesses nothing**: no headroom it was not
told, no holders it was not given.

**A model that is already resident needs no room to start**, so no
headroom is reported against it: the gate's free-VRAM half does not
apply then, and reporting a shortfall that is not one would be a lie
of the same kind this change fixes.

## The tests

Four, in `crates/hs-mcp/src/main.rs::backend_reason_tests`:

* `a_shortfall_is_reported_with_both_numbers` — the free VRAM *and*
  the headroom, both in GiB.
* `the_holders_are_named` — the holders and their sizes.
* `a_resident_model_reports_no_headroom_requirement` — a resident
  model reports no requirement, because it needs none.
* `a_body_with_no_numbers_falls_back_to_the_symptom` — a health body
  with no numbers still says something true.

The last one matters: the diagnosis must never be a *guess*. When the
numbers are absent, the line says what it always said.

## Timings

The change adds two fields to one JSON body and a string to one status
line. The health probe is cached (`BACKEND_PROBE_TTL`), so the holders
summary — which the server already computed for its own log line — is
reused rather than recomputed. The status call is unchanged in cost.

## How the numbers were produced

```sh
# the refusals, from the tier's journal
ssh big 'journalctl -u hs-serve-scribe-olmocr.service --since "12 hours ago" --no-pager'
# the dashboard, before and after
python3 /tmp/status_probe.py <the old or the new hs-mcp>
# the whole diagnosis, against a scribe server that sends the new payload
python3 /tmp/local_diagnosis.py
```

The live services were not upgraded or restarted: this iteration's
rules say not to run `hs upgrade` on live services. The new binaries
were run side by side — the new scribe server on `:7439` and the new
MCP by hand — against the same corpus and the same GPU.
