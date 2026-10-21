# Run 2026-10-21-07 — the status that says "not running" about a running server

## Problem, measured

`hs distill status` reports the distill server's liveness from a **PID
file**. The live server on `big` runs under **systemd**, so no PID file
is written for it — and the file that is there is from **April 14**,
naming a PID that no longer exists.

Measured on `big`, 2026-10-21, the live server serving 445,451 points:

```
$ hs distill status
Qdrant: OK (http://localhost:6333)
Server: not running          <-- WRONG
Collection: academic_papers
Points: 445451               <-- fetched FROM that server, one line later
Device: Cuda
```

**The output contradicts itself in one screen.** The command says the
server is not running, and then reports the collection, the point count
and the compute device — data it can only have fetched *from that
server*. The server answers `/health` with
`{"status":"ok","compute_device":"Cuda",...}` and is serving searches
while the command says otherwise.

The live server is real: PID 2227527, `/home/ladvien/.local/bin/hs-distill-server`,
started by `hs-serve-distill.service`.

**This is the project's own anti-pattern.** `CLAUDE.md` says:

> *Watcher liveness is heartbeat-based*, not PID-file-based. When
> diagnosing "is the watcher alive?" check `last_tick_seconds_ago` from
> status, not `/var/run/*.pid`.

The rule was written for the watchers. The distill server's *status
command* uses the pattern the rule forbids, and the consequence is the
same: a wrong answer to "is it running?".

**Why it matters to a user.** `hs distill status` is the documented
verification step of the deployment runbook
(`docs/deployment.md:1041`: *"Verify with `hs distill status` and
`hs status`"*) and of `crates/hs-distill/README.md`. An operator who
runs it to check the server after a deploy is told the server is down
while it is up. The wrong answer is worse than no answer: it sends the
operator to restart a healthy server.

**A second, smaller wrongness looked for and *not* found.** The
`scribe` command's enum has a `Status` variant; it looked like a
declared-but-never-dispatched branch — a command advertised in `--help`
that does not exist. It is not: the variant belongs to `InboxAction`
(`crates/hs/src/scribe_cmd.rs:105`), it **is** dispatched
(`crates/hs/src/scribe_inbox.rs:355`), and `hs scribe inbox status`
works — it asks systemd for the unit's state, which is the right
signal. This was checked rather than assumed, and it changed nothing.

## Why this ranks first

Against the runner-up — **cross-encoder reranking**, which was the
backlog's first item and whose blocker (the shared 3090) has just
cleared: 18 GB of the 24.6 GB is free.

It ranks **above** it because of what each buys:

* The reranker is **not present in home-still at all**. There is no
  rerank code path, no client, no service, no endpoint. The model is on
  disk (`~/models/bge-reranker-v2-m3-GGUF/bge-reranker-v2-m3-Q8_0.gguf`)
  and no service answers on the reranker's port. Building it is a new
  GPU service, a new serving path, a new latency budget, and a new
  failure mode — and it would land on a **shared** GPU that another
  agent's job took 12 GB of earlier today.
* The status wrongness is a **definite bug**, not a tuning problem: a
  command reports a healthy service as dead, in the runbook's own
  verification step.

The honest ranking by user impact, though, is: wrong answers >
silent data problems > reliability > missing features. The reranker
would improve the *order* of results; this fix removes a **false
statement** from a command operators use to decide whether to act.
A wrong report about liveness sends an operator to restart a healthy
server — and restarting the distill server under load is exactly when
that is most costly.

## The technique, from the corpus

Searched home-still on itself first, and the corpus holds the answer
directly. *Software Architecture in Practice*, 4th edition
(`bass_software_architecture_in_practice_4th`), in the section on
system monitoring, names the tactic and the failure mode:

> **Ping/echo.** *"an asynchronous request/response message pair is
> exchanged between nodes; it is used to determine reachability and the
> round-trip delay through the associated network path. In addition, the
> echo indicates that the pinged component is alive. The ping is often
> sent by a system monitor. Ping/echo requires a time threshold to be
> set…"*

and, on the watchdog variant of the same idea:

> *"When the detection mechanism is implemented using a counter or timer
> that is periodically reset, this specialization of the system monitor
> is referred to as a **watchdog**. During nominal operation, the
> process being monitored will periodically reset the watchdog
> counter/timer as part of its signal that it's working correctly."*

Two things follow, and they are the design:

1. **The signal is a request/response against the service**, not a
   file on disk. The distill server already has exactly that endpoint:
   `/health` returns the server's own verdict on its state, its
   collection, its device and its version.
2. **The same text names the other half of the defect**: the "dead"
   PID file is a *stale* watchdog — a counter that was never reset,
   because the process that would have reset it is not the one running.

*Designing Data-Intensive Applications* (also in the corpus) frames
the same point for replicated systems: a component's liveness is
established by **asking it**, because only the component knows whether
it is making progress.

The project already applies this rule. `hs status` — the main
dashboard — probes each service's health endpoint and reports the
result, and it reports the distill server correctly today
(`● distill http://192.168.1.110:7434 idle (Cuda) 0.0.1-rc.373`).
**The dashboard is right and the per-service command is wrong.** The
fix is to make the per-service command use the same signal the
dashboard already uses.

## The technique chosen

**Ask the server whether it is alive, and report the answer — with the
PID as an extra, never as the verdict.**

Concretely, `hs distill status` will:

* **ping the server** (the `/health` request the command *already*
  makes, one line later, to fetch the collection and the point count)
  and report `running` when it answers;
* when the ping fails, report `not running` — and say **where** it
  looked, so the failure is actionable;
* report the **PID** as additional information.

**The PID comes from the server itself.** The server now includes its
own process id in `/health` (`"pid": 2257468`). This is the only party
that knows it: systemd, launchd and a hand-typed command all start the
server differently, and a client that guesses — from a PID file, or by
scanning the process table — guesses wrong for at least one of them.
The project already had the pattern: `restart_cmd` asks **systemd** for
`MainPID` rather than reading a file. Asking the server is the same
idea, and it is the one that works when the server was not started by
systemd.

The PID file is not consulted for liveness at all. It is a *stale
artefact* of a launch path that no longer runs the server that way, and
reading it is what produced the wrong answer.

**The signal is one path.** There is no "if the PID file says X and
the health check says Y, prefer Y" — the health check *is* the
verdict, because it is the only signal that is about the service rather
than about a file. A server that answers `/health` is running, whether
or not systemd, launchd, or a hand-typed command started it.

**The wire format grows by one optional field.** `HealthResponse` gains
`pid: u32` with `#[serde(default)]`, so a new client reads an old
server's health (no pid, reported as "running" without one) and an old
client reads a new server's health (the extra field is ignored). The
`/health` contract is otherwise untouched, and `/health` is
unauthenticated — it says *that* a dependency is down, never why.

**The one thing this does not change.** `hs status` — the main
dashboard — already probes each service's health endpoint and already
reported the distill server correctly. It was only the per-service
command that was wrong, and it was wrong in the opposite direction from
the dashboard, which is what made the inconsistency so easy to miss.

## Expected effect

* `hs distill status` on a live server says `running`, and names the
  process it found;
* the output no longer contradicts itself — one signal, one verdict;
* a server that is genuinely down says `not running` and says which
  address it probed;
* the `ScribeCmd::Status` variant either works or is removed, so
  `--help` does not advertise a command that does not exist.

## What this is not

* Not a new service, a new dependency, or a new GPU cost. It reuses
  the request the command already makes.
* Not a change to the server. The `/health` endpoint and its contract
  are untouched.
* Not a change to the watcher liveness rule, which is already
  heartbeat-based and already correct.
