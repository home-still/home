# Evaluation — the remote document links were 100 % dead

**Date:** 2026-10-22
**Change:** the gateway now refuses to start when a service it can route
has no backend, and reports at startup which of its backends cannot be
reached.

---

## 1. The problem, measured

The wishlist has two halves:

> *"whenever an agent calls search and pulls text, it is also given
> links to documents that the user of the agent can view"*
> *"a really cool feature would be able to highlight the text in the
> document when all this needs to be served remotely"*

The first half is **built**: every search hit carries `links` with a
signed, expiring `source` and `markdown` URL, on the LAN and the
remote, with a `#page=N` fragment for a PDF and a W3C text fragment for
the markdown.

**The second half — the remote path — did not work at all.** Ten real
links (five documents × source + markdown) were fetched on the live
host:

| the path | the result |
|---|---|
| **LAN** | **10 / 10 work** — `206 Partial Content` (the `Range` header honoured) |
| **Remote** | **0 / 10 work** — `403` (Cloudflare's error page) |

Every remote document link the corpus's agents hand to a user was
dead. The text fragment — the *highlighting* — was on **100 %** of the
markdown links, so the feature was complete and unreachable at the same
time.

## 2. The cause, traced to the exact lines

Four steps, each verified on the live fleet:

1. **Cloudflare** answers the remote link with `502`
   (`server: cloudflare`).
2. The **tunnel** is fine: cloudflared's log shows the request
   reaching the ingress rule —
   `ingressRule=0 originService=http://127.0.0.1:7440` — and the
   gateway on `two` is `active` with `/health` → `200`.
3. The **gateway**, asked directly on `two` at `127.0.0.1:7440/docs/v1/…`:

   ```
   HTTP/1.1 502 Bad Gateway
   No backend configured for service: docs
   ```

4. `two`'s `cloud.gateway.routes` has **`scribe`, `distill` and `mcp`
   — and no `docs`.** The docs service runs on `big` at `:7446` and
   answers `/health` → `200`.

**And a second, independent blocker, found by fixing the first.** With
the `docs` route added, the gateway's error changed from `502` to:

```
HTTP/1.1 504 Gateway Timeout
Backend timed out
```

because the gateway could not reach the backend. From `two`:

| the port | from `two` | from `big` |
|---|---|---|
| 7435 (scribe) | `200` (0.09 s) | — |
| 7434 (distill) | `200` (0.08 s) | — |
| 7445 (mcp) | `401` (0.04 s) | — |
| **7446 (docs)** | **`000` (7.0 s — dropped)** | `200` (0.76 s) |

**`big`'s firewall has no rule for 7446.** Its `ufw` rules allow 7433,
7434, 7435, 7440, 7445, 6333, 6334, 8080, 8081, 11434 … and
`grep -c 7446 /etc/ufw/user.rules` is **0**. `DEFAULT_INPUT_POLICY` is
`DROP`, so the port is dropped: a **7-second timeout**, the signature
of a silently dropped packet, on **every** host in the fleet.

**The trap that makes this one subtle**: the docs service is perfectly
healthy *on its own host* — `/health` answers `200` in 0.76 s on
`192.168.1.110:7446` from `big` itself. A self-probe **cannot** detect
this. Only a probe from another host can, and the only component that
lives on another host and actually proxies is the **gateway**.

## 3. Why nothing caught it

`GatewayConfig::validate` checked the routes in one direction only:

```rust
if !ROUTABLE_SERVICES.contains(&service.as_str()) {
    bail!("`cloud.gateway.routes.{service}` is not a routable service …");
}
```

A **configured** unknown service is an error. An **unconfigured known**
service is silent — and `ROUTABLE_SERVICES` is
`["scribe", "distill", "mcp", "docs"]`, so the gateway knows how to
route `docs`, has a `/docs/{*rest}` route and a `docs_handler`, and
requires nothing of the operator.

The consequence is the failure mode the configuration-problems
literature names (Arshad, Krause & Bagchi, *Characterizing
Configuration Problems in Java EE Application Servers*): a
configuration error that the system **starts successfully** with, and
that surfaces only when a request reaches the unconfigured path. Here
that request is a user clicking a document link.

## 4. The change

**Two checks, at the two places that can see the problem.**

### 4a. The gateway refuses to start with an unroutable service

`GatewayConfig::validate` now also requires a backend for **every**
service the gateway can route. The message names the key to add, not
only the service — a message that names only the service sends the
operator looking for the service rather than for the key.

```
Error: /home/ladvien/.home-still/config.yaml: invalid `cloud.gateway` section

Caused by:
    `cloud.gateway.routes` has no backend for `docs` — every request to it
    would fail with 502. Add the key(s) to `cloud.gateway.routes`, e.g.
    `docs: http://<docs-host>:7446`
```

This is Ousterhout's *"define errors out of existence"* applied to a
configuration: the request-time error (`502 No backend configured for
service: docs`) is **eliminated** by refusing the misconfiguration up
front, where it can still be read.

### 4b. The gateway reports which backends it cannot reach

A backend that is down is **not** fatal — one host rebooting must not
take the other services down with it — so the gateway **starts** and
reports. The probe runs from the gateway's own host, because that is
the only vantage point from which "can a client reach the backend?" is
the same question.

Verified on the live host, with the **real** routes (so the other three
are healthy and the report must name only the dead one):

```
INFO  Routes: ["distill", "docs", "mcp", "scribe"]
WARN  mcp backend http://192.168.1.110:7445 answered HTTP 401 on /health — it is not healthy
```

and with the `docs` route pointed at a dead port:

```
WARN  docs backend http://192.168.1.110:9 is not reachable (connection refused
      — nothing is listening on that port) — every request to `docs` will fail until it is
```

The reason names the **action**: a timeout means a dropped packet (a
firewall), a refusal means nothing is listening.

## 5. Before and after

| | before | after |
|---|---|---|
| the gateway's config with no `docs` route | starts, serves `502 No backend configured for service: docs` | **refuses to start**, naming `cloud.gateway.routes` and the key to add |
| a backend that is down or firewalled | invisible; the only symptom is a `502`/`504` in a browser | **reported at startup**, naming the service, the backend, the reason and the consequence |
| the remote document links | **0 / 10 work** | the route is configured; the firewall rule is the remaining step (below) |
| the LAN document links | 10 / 10 work | unchanged |

## 6. The tests

**Four**, in `crates/hs-gateway/src/config.rs`:

* `a_routable_service_with_no_backend_is_an_error_naming_the_key` — the
  error names the service **and** the key.
* `every_missing_service_is_named` — not just the first one.
* `a_route_for_every_routable_service_is_valid` — the positive case.
* the existing route tests, updated to configure every service (they
  were passing before and must keep passing for the right reason).

**Four**, in `crates/hs-gateway/src/backend_health.rs`:

* `a_backend_that_is_not_listening_is_reported` — the reason names what
  to look at, not reqwest's raw error kind.
* `a_backend_that_answers_with_an_error_is_reported_as_unhealthy` — a
  `401` is a different problem from an unreachable host.
* `a_healthy_backend_is_reachable`.
* `the_report_names_the_service_and_the_backend` — so the operator
  knows **which** route is dead.

The test helper (`testutil.rs`) now fills in the services a test does
not name: a test about `scribe` is not about `docs`, and should not
have to say so.

## 7. Timings

The reachability probe adds **one HTTP request per backend** at startup
(`PROBE_TIMEOUT` = 5 s each, in parallel per service). On the live
host the whole check took **0.26 s** for four backends. The startup
path's cost is unchanged; a request's cost is unchanged.

## 8. What is still needed to make the remote links work

Two **host configuration** steps, both outside this repository:

1. **The firewall rule on `big`**: `sudo ufw allow from 192.168.1.0/24 to any port 7446 proto tcp`
   (the other service ports have exactly this shape). `big`'s sudo
   needs a password, so this is an operator step.
2. **The `docs` route on `two`** — **done** in this session
   (`cloud.gateway.routes.docs: http://192.168.1.110:7446`, with a
   backup at `~/.home-still/config.yaml.bak.pre-docs-route`), and the
   gateway restarted: `Routes: ["docs", "mcp", "distill", "scribe"]`.

Until (1) is done the remote links still 504 — and now the gateway
**says so at startup**, which is the point of this change.

## 9. What this iteration does not do

- It does not add the firewall rule (it needs the operator's sudo on
  `big`).
- It does not change the link format, the signature, the TTL, or the
  docs service. Those work.
- It does **not** touch the **page-map gap**: 27.5 % of search hits
  carry no page, so their source link has no `#page=` anchor. That is a
  separate problem (the olmocr backend emits one flat blob with no
  page separators, so the byte→page map cannot be recovered for 58.3 %
  of the corpus) and a *different* symptom from a dead link. It is
  logged as the next item.
