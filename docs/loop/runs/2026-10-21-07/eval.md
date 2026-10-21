# Evaluation — the status that says "not running" about a running server

Measured on the live corpus on `big`, 2026-10-21.

## The problem

`hs distill status` reported the distill server's liveness from a **PID
file**. The live server on `big` runs under **systemd**, so no PID file
is written for it — and the file that is there is from **April 14**,
naming a PID that no longer exists.

The output before the change:

```
$ hs distill status
Qdrant: OK (http://localhost:6333)
Server: not running          <-- WRONG
Collection: academic_papers
Points: 445451               <-- fetched FROM that server, one line later
Device: Cuda
```

**The report contradicted itself in one screen.** It said the server was
not running, and then reported the collection, the point count and the
compute device — data it can only have fetched *from that server*.

The live server was real: PID 2227527,
`/home/ladvien/.local/bin/hs-distill-server`, started by
`hs-serve-distill.service`, serving 445,451 points and answering
`/health` with `{"status":"ok","compute_device":"Cuda",…}`.

**The project's own rule says this is the wrong way to ask.**
`CLAUDE.md`:

> *Watcher liveness is heartbeat-based*, not PID-file-based. When
> diagnosing "is the watcher alive?" check `last_tick_seconds_ago` from
> status, not `/var/run/*.pid`.

The rule was written for the watchers; the distill server's *status
command* used the pattern the rule forbids, with the same consequence.

**Why it matters.** `hs distill status` is the documented verification
step of the deployment runbook (`docs/deployment.md:1041`) and of
`crates/hs-distill/README.md`. An operator who runs it to check the
server after a deploy is told the server is down while it is up — and
the wrong answer is worse than no answer, because it sends them to
restart a healthy server.

## The change

**The server's own answer is the verdict.**

* The server now includes its **own PID** in `/health`
  (`"pid": 2257468`). It is the only party that knows it: systemd,
  launchd and a hand-typed command all start it differently, and a
  client that *guesses* — from a PID file, or by scanning the process
  table — guesses wrong for at least one of them. The project already
  had the pattern: `restart_cmd` asks **systemd** for `MainPID` rather
  than reading a file.
* `hs distill status` **pings** the server — the `/health` request it
  was already making, one line later — and reports `running` when it
  answers.
* When the ping fails, it reports `not running` **and says where it
  looked**, so the failure is actionable.
* The PID file is not consulted for liveness at all.

**The signal is one path.** There is no "if the PID file says X and
the health check says Y, prefer Y". The health check *is* the verdict,
because it is the only signal that is about the service rather than
about a file.

**The wire format grows by one optional field.** `HealthResponse` gains
`pid: u32` with `#[serde(default)]`: a new client reads an old server's
health (no pid, reported as "running" without one), and an old client
reads a new server's health (the extra field is ignored). The `/health`
contract is otherwise untouched, and `/health` is unauthenticated — it
says *that* a dependency is down, never why.

## Result

The **new** CLI against the **live** rc.373 server (which does not yet
report a pid, and does not need to):

```
$ hs distill status --server http://127.0.0.1:7434
Qdrant: OK (http://localhost:6333)
Server: running
Address: http://127.0.0.1:7434
Version: 0.0.1-rc.373
Collection: academic_papers
Points: 445451
Device: Cuda
```

The new CLI against the **new** server (which reports its own pid):

```
$ hs distill status --server http://127.0.0.1:7437
Qdrant: OK (http://localhost:6333)
Server: running (PID 2257468)
Address: http://127.0.0.1:7437
Version: 0.0.1-rc.375
Collection: academic_papers
Points: 445451
Device: Cuda
```

**The reported PID is the real process's PID.** Verified against the
process table:

```
$ ps -o pid=,cmd= -p 2257468
2257468 /home/ladvien/hs-target-07/debug/hs-distill-server --port 7437
```

The failure path names the address and the reason:

```
$ hs distill status --server http://127.0.0.1:1
Qdrant: OK (http://localhost:6333)
error: Server: not running (no answer at http://127.0.0.1:1/health: …
      Connection refused (os error 111))
Collection: unavailable (server at http://127.0.0.1:1 not reachable: …)
```

| | before | after |
|---|---|---|
| the live server's liveness | **"not running"** (wrong) | **"running"** |
| the PID reported | none (a stale file was read) | **the server's own PID** |
| a dead server's report | "not running", no address | "not running", **with the address and the reason** |
| the report contradicts itself | **yes** | **no** |

## Verified end to end, on the live corpus

The command was run against the **live** distill server on `big`, reading
the **real** 445,451-point collection, with the real Qdrant and the
real CUDA embedder. The new binary was run on an alternate port
(`:7437`) against the same Qdrant; the live service on `:7434` was
never touched.

**The search path is untouched.** The same new server served searches
while the status was taken:

```
$ hs distill search "oxytocin social behaviour" --server http://127.0.0.1:7437
1. Flibanserin: A Miracle Drug in Management of Hypoactive Sexual Desire
   Disorder in Female … [score: 0.500] (2020)
2. CNS-16-e92 [score: 0.333]
   CNS-16-e92:295-301 (page 14)
```

## Timings

The status command now makes **one** HTTP request to the server
(`/health`) and one to `/status` — the same two it made before, except
that the liveness line is now decided by the first of them instead of
by a file read. A file read is not a network round trip, so the
command's cost is unchanged.

Measured, 5 runs each, against the live server on the same host:

| | median |
|---|---|
| the new CLI | **392 ms** |
| the released CLI (rc.373) | **372 ms** — and this figure *includes* the ssh round trip |

The two are the same within the noise, and the released figure is
measured through ssh, so it carries an overhead the new one does not.

## The tests

Three tests, in `crates/hs/src/distill_cmd.rs::status_tests`, built on
the project's `FakeServer`:

* `a_server_that_answers_is_reported_as_running` — a server that
  answers `/health` is reported as `running`, and the **PID it
  reported** is the PID shown.
* `a_server_that_does_not_answer_is_reported_as_not_running` — a
  server that does not answer is reported as down, **and the report
  says where it looked**.
* `the_report_does_not_contradict_itself` — the exact defect: a report
  that says "not running" must not also report the collection's point
  count.

The third test is the one that would have caught this bug: it asserts
the *invariant* the old output violated, not the shape of the new
output.

## What was checked and found not to be wrong

The `scribe` command's enum has a `Status` variant that looked like a
declared-but-never-dispatched branch. It is not: the variant belongs to
`InboxAction`, it is dispatched
(`crates/hs/src/scribe_inbox.rs:355`), and `hs scribe inbox status`
works — it asks systemd for the unit's state, which is the right
signal. Checked rather than assumed; it changed nothing.

## How the numbers were produced

```sh
# the live server, before (the released rc.373 CLI)
hs distill status

# the new CLI against the live server
./target/debug/hs distill status --server http://127.0.0.1:7434

# the new CLI against the new server, which reports its own pid
./target/debug/hs distill status --server http://127.0.0.1:7437
```

The live service on `:7434` was never upgraded: this iteration's rules
say not to run `hs upgrade` on live services. The new binary was run
side by side on an alternate port against the same Qdrant, and the live
server was left as it was.
