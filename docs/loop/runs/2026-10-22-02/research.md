# Run 2026-10-22-02 — the status that says "backend unavailable" and nothing else

## Problem, measured

On `big`, the `hs status` dashboard showed this, live, on 2026-10-22:

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
builder **drops all three**, replacing them with the string
`"backend unavailable"`. The information is fetched, parsed, and
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

So the tier refused more often than it passed, and the dashboard never
said a word about it.

**The arithmetic, for the record.** The card is 24,576 MB. The distill
server's bge-m3 embedder holds 7,534 MB resident, the scribe server
2,596 MB, and the olmocr backend wants ~15.4 GB at
`--gpu-memory-utilization 0.65` (the `llama-swap.yaml` comment says
0.60 — the two disagree). The free VRAM with only our own services
resident is 14,446 MB, and the gate wants 13,500: **a surplus of
946 MB**, which is smaller than the `bench_elements` process (591 MB)
another agent runs. **Any** tenant over 946 MB makes the tier refuse.

## Why this ranks first

Against the runner-up — **cross-encoder reranking**, the backlog's first
item, whose GPU blocker has cleared — this wins on the user's own
ranking of harm, and on a different axis from the last three
iterations:

* A cross-encoder *reorders* the results of a search that works.
  Measured on this corpus, 2026-10-21: it moved the expected passage's
  rank on **4 queries better and 3 worse**, for a **5× latency
  increase** (162 → 776 ms median), and the sibling crate's own M37
  measured the same null (+0.78 points, CI [−3.14, +4.71]).
* This is a **blind spot in the only dashboard an operator has**. The
  corpus is not growing on one of two scribe tiers, and the tool that
  says so says "backend unavailable" — which is indistinguishable from
  a crashed server, a missing model, a wrong URL, or a broken
  converter. The operator cannot act on it: the next step is to read a
  journal on another host.

**It is also issue #10's fourth bullet**, which another agent filed on
2026-10-21 with the same diagnosis: *"Report the tier's VRAM gate in
`system_status`. `free_vram_mb` and `model_resident` are already logged
internally … surfacing them in the status payload would make this
diagnosable without reading another host's journal."*

Of issue #10's four bullets, this is the one that is **in this
repository**: the other three are about the host's `llama-swap.yaml` and
`run-olmocr.sh` (not in the repo), the embedder's `idle_release_secs`
(a config change on a shared box), and the cold-start deadline.

## The technique

Searched the corpus first, and it holds the principle twice.

**Observability is the property that matters.** *A Philosophy of
Software Design*, on the difference between a fault and a failure:

> *"The notion of 'observability' is critical here: If a failure could
> have been observed, then it is a failure, whether or not it was
> actually observed."*

And, immediately after:

> *"Distinguishing between faults and failures allows us to discuss
> repair strategies. If code containing a fault is executed but the
> system is able to recover from the fault without any observable
> deviation from the otherwise specified behavior, we say that no failure
> has occurred."*

**A diagnosis must map to an action.** *Agentic Artifact Creation* uses
"failure observability" and "**repairability**" as its lens: they ask
*"how far decisions constrain other state, whether failures become
visible and attributable in time, and whether a diagnosis maps to a
bounded action that preserves accepted work."*

**The gap the survey names.** *AI Agent Systems* on why agent-facing
systems are hard to operate: the failures *"often only surface under
integration tests or production-like workloads"*, and agents must cope
with *"partial observability (incomplete logs, truncated error
output)"*.

So the technique is: **report the cause, not the symptom — and make the
cause a bounded action.** A status line that says "backend unavailable"
is a symptom; a status line that says *"12.5 GiB free, needs 13.2 GiB —
another process is holding the card"* is a diagnosis, and it maps to an
action: free the card, or lower the headroom.

## The change

**The scribe's status carries the reason, and the reason is the VRAM
gate with its numbers.**

1. **The scribe server's `/health` names the gate.** It already reports
   `backend_vram_free_mb`, `backend_model_resident` and
   `backend_checked_at`; it does not report the **headroom it wants**,
   so a reader cannot tell "12.5 GiB free" from "12.5 GiB free but
   13.2 GiB is needed". The headroom goes into the payload.

2. **`hs status` and the MCP's `system_status` show it.** The activity
   string becomes a diagnosis: *"backend unavailable: 12.5 GiB free,
   needs 13.2 GiB"* — the number and the requirement, side by side.

3. **The holders are named when the gate is the reason.** The scribe
   already computes `compute_apps_summary_async()` for its own log
   line; that summary goes into the payload too, so the status says
   *who* is holding the card.

**The model is resident.** When the model is already loaded the
free-VRAM half of the gate does not apply, and the status says so
rather than reporting a shortfall that is not one.

**What this is not.** Not a change to the gate: the gate is *correct* —
every one of the 54 refusals had a genuine shortfall, and refusing
work the card cannot do is right. Not a config change on the host. Not
a new endpoint: the numbers are already parsed and discarded, so this
is a change to what is *reported*, not to what is *fetched*.

## Expected effect

* `hs status` names the shortfall: the free VRAM, the headroom, and
  the holders — so the operator learns the cause from the dashboard
  instead of from another host's journal;
* the MCP's `system_status` carries the same, so an **agent** can say
  why a tier is idle rather than reporting a bare "backend
  unavailable";
* the gate's behaviour is unchanged, so nothing that works today stops
  working.
