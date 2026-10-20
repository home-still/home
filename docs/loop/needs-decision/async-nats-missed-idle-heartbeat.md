# The async-nats "missed idle heartbeat" crash-loop

**Status:** open — needs a decision. Not a home-still bug, and not fixed here.

## What happens

The `hs distill watch-events` and `hs scribe watch-events` daemons die with:

```
event: closed
error: Error: event delivery for scribe.completed failed: the consumer is no
longer receiving: jetstream delivery error: missed idle heartbeat
```

and systemd restarts them. Measured on `big` (2026-10-20): 19 deaths in 48 h
across three watcher units, systemd's restart counter at 120 for the distill
one. Every watcher on the host dies at the *same millisecond*, so the trigger
is shared, not per-process.

## Root cause (measured, not inferred)

`async-nats` 0.47.0, `jetstream/consumer/pull.rs`:

1. The broker sends `100 Idle Heartbeat` **only while a pull request is
   outstanding**. After the pull expires (the `408 Request Timeout` frame)
   it sends **nothing** until the client re-publishes. Verified directly
   against the live broker: zero frames for 70 s without a re-publish.
2. `Consumer::messages()` requests `idle_heartbeat: 15s`. The pull
   `Stream::poll_next` treats `2 × 15s = 30s` of silence as
   `MessagesErrorKind::MissingHeartbeat` and aborts the consumer.
3. The re-publish is driven by `request_tx.send(())` on a
   `tokio::sync::watch(1)` channel inside the request task. That task's
   `select!` has an arm — `context.client.state.changed()` — that can
   `continue` and skip the re-publish for one iteration. Each skip costs a
   full 30 s of broker silence.

**Diagnostic tells** (each one rules something out):

- The log says `event: closed`, **never** `event: disconnected`. A network
  drop would log `disconnected`. So it is not a network drop.
- The broker never restarts (`NRestarts=0`) and has zero load.
- The live consumer config matches what `ensure_consumer` wants — no drift.
- `num_waiting` stays > 0: the pull request is still outstanding.

## Why it is not fixed here

The brief says: *"A change to a shared or vendored crate (the async-nats
crash-loop fix is an example) is not yours to make."* `async-nats` is a
third-party dependency, and the fix would have to be upstreamed or vendored
and patched — a decision about a dependency, not a feature of this project.

## What *is* true today

The crash is a clean exit 1, not data loss:

- The durable consumer is never deleted, and `ensure_consumer` uses
  `get_or_create_consumer` + `update_consumer`, so no message is lost and no
  delivery count is reset.
- systemd restarts the daemon, which reconnects and continues.
- The messages stay in the stream (`num_pending` 0, but 349 messages held in
  `SCRIBE` are delivered on the next connect).

So the pipeline self-heals; it is a noisy restart, not a stall.

## Options

**A. Catch `MissingHeartbeat` and recreate the pull stream** (recommended).

The error arrives as an `Err` item on the event stream
(`hs-common/src/event_bus/nats.rs::surface_errors` already surfaces it as
the last item). The watcher could treat *this one error kind* as a
reconnect signal: drop the stream, rebuild the consumer handle, and carry
on — instead of exiting and letting systemd restart the process.

- Cost: one branch in `NatsBus::consume` plus a retry loop, in **this**
  repo, against a public API of the dependency. No patching upstream.
- Effect: the daemon stops crash-looping entirely. The restart counter
  stops climbing; the log stops filling with errors.
- Risk: the retry must be bounded, or a genuinely dead broker turns into an
  infinite loop. It must also not spin: the upstream error means the
  connection is gone, so the retry has to go through a fresh connection.
- This is the "one path" shape: the watcher reconnects, it does not fall
  back to a degraded mode.

**B. Configure the consumer so the broker keeps heartbeating.**

Set `max_expires` (and, for a push consumer, `idle_heartbeat`) on the
consumer config so the broker's heartbeat outlives the pull-request gap.
`ensure_consumer` builds a `PullConfig` today; adding `max_expires` is a
small change in this repo.

- Cost: small, entirely in this repo.
- Effect: unknown until measured. The broker's heartbeat is driven by the
  pull request's `idle_heartbeat`, and the failure is the *re-publish* not
  happening — a longer server-side lease may not close that window.
- Risk: it may look like a fix while the underlying race survives, which
  is worse than the loud failure we have now.

**C. Patch async-nats upstream.**

Fix the `select!` so the re-publish cannot be skipped (e.g. re-arm the
request after the `state.changed()` arm), and/or make the client tolerate a
missed heartbeat by re-issuing the pull instead of aborting.

- Cost: an upstream PR, or a vendored fork with a patch step in the build.
- Effect: the correct fix, and it removes the failure for every user of the
  dependency.
- Risk: the fork/patch has to be maintained across dependency bumps.

## Recommendation

**A**, with **B** measured alongside it, because A is in this repo and
addresses the failure mode directly (the client aborting a consumer over a
transient heartbeat gap), while B is a one-line experiment that either
helps or does not.

Whichever is chosen, it should be measured against the same evidence this
note is based on: the 18-query set in `docs/loop/queries.yaml` is about
retrieval, so the crash-loop needs its own harness — a watcher started
against a live broker, with the broker's pull request deliberately
expiring, showing the watcher survives and keeps consuming.

## Related

- The `event: closed` ordering (the client is dropped *before* the error is
  logged) is still unexplained. It does not affect the diagnosis — the
  error is what ends the process — but a future reader should not assume the
  log order shows the cause.
