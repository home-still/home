# Research — the remote document links do not work

**Date:** 2026-10-22
**Question:** the wishlist — *"whenever an agent calls search and pulls text, it is also given links to documents that the user of the agent can view"* and *"highlight the text in the document when all this needs to be served remotely"*. The first half works. The second half does not. Why, and what is the smallest change that makes it work?

---

## 1. What already works (measured, not assumed)

The links are already minted, signed and served. A live `distill_search`
returns, on every hit:

```
  source.lan:     200 application/pdf 4281893 bytes
  source.remote:  HTTP 403 Forbidden
  markdown.lan:   200 text/markdown; charset=utf-8 143012 bytes
  markdown.remote: HTTP 403 Forbidden
```

and the payload carries everything a link and a highlight need:

```
  the keys: ['authors', 'category', 'chunk_text', 'doc_id', 'doi',
             'line_end', 'line_start', 'links', 'page', 'score',
             'title', 'year']
```

**The text fragment is on 100 % of the markdown links** (measured over
80 hits from 8 queries) — so the *highlighting* half of the wishlist
works, on the LAN. The fragment is a W3C text fragment
(`#:~:text=3%5D%20uses%20both%20audio%20and`), and the source link
carries `#page=N` when the chunk's page is known.

**The retrieval is in excellent shape**: the fixed query set scores
**RECALL@10 25/25 = 100 %** (concept 8/8, literal 10/10, named 7/7),
median latency 236 ms, and only 4 % of first hits are a reference list.
So this iteration is not about finding passages.

## 2. The defect, measured

The two link paths were fetched over 10 real links (5 documents ×
source + markdown):

| the path | the result |
|---|---|
| **LAN** | **10 / 10 work** — `206 Partial Content` (the `Range` header honoured) |
| **Remote** | **0 / 10 work** — `403` (via Cloudflare) |

**The remote path is 100 % broken.** The wishlist's second half — "when
all this needs to be served remotely" — does not work at all.

### The cause, traced to the exact line

1. `curl` the remote link through Cloudflare → **HTTP 502**, `server:
   cloudflare`. Cloudflare's 502 is what the browser sees as a 403/502
   error page.
2. The tunnel is fine: `cloudflared`'s log shows the request reaching
   the ingress rule — `ingressRule=0 originService=http://127.0.0.1:7440`
   — and the gateway on `two` is `active` with `/health` → `200`.
3. Ask the gateway **directly**, on `two`, at `127.0.0.1:7440/docs/v1/…`:

   ```
   HTTP/1.1 502 Bad Gateway
   No backend configured for service: docs
   ```

4. The gateway's config on `two`:

   ```yaml
   cloud:
     gateway:
       routes:
         scribe: http://192.168.1.110:7435
         distill: http://192.168.1.110:7434
         mcp: http://192.168.1.110:7445
   ```

   **There is no `docs` route.** The docs service is running on `big`
   at `:7446` and answers `/health` → `200`; the gateway simply does
   not know about it.

### Why nothing caught it

`GatewayConfig::validate` (`crates/hs-gateway/src/config.rs:198`) checks
that every **configured** route is a *known* service:

```rust
if !ROUTABLE_SERVICES.contains(&service.as_str()) {
    bail!("`cloud.gateway.routes.{service}` is not a routable service ...");
}
```

`ROUTABLE_SERVICES` is `["scribe", "distill", "mcp", "docs"]` — so
`docs` is a service the gateway **knows how to route** (it has a
`/docs/{*rest}` route, a `docs_handler`, and a `docs` entry in the
balancer) but which nothing **requires** to be configured. The
validation is one-directional: a configured unknown service is an
error; an unknown-to-the-config but known-to-the-code service is
silent.

**The consequence**: the gateway starts, passes its own validation, and
serves `502 No backend configured for service: docs` to every user who
clicks a document link — with a message that names the service but not
the **key to add**.

## 3. The corpus's own answer

The corpus has the relevant literature, and it is unusually direct.

### *A Philosophy of Software Design* (Ousterhout, 2018), §10.3

> **"Define errors out of existence.** The best way to eliminate
> exception handling complexity is to define your APIs so that there are
> no exceptions to handle: define errors out of existence."

Retrieved from the corpus (`ousterhout_philosophy_of_software_design`,
page 1). The same book's argument about **partial information hiding**
names the fix: the caller should not have to know that a defaulted
item exists, but when they do need to override it, the interface must
tell them how.

Here the *caller* is an operator reading a 502, and the *thing that
must not exist* is the error — because a correctly configured gateway
has no "no backend for docs" error to raise at all.

### *A Philosophy of Software Design*, §9.9 (the module-shape argument)

> "The decision to split or join modules should be based on complexity.
> Pick the structure that results in the best information hiding, the
> fewest dependencies, and the deepest interfaces."

The gateway's config is the *deep interface* between the operator and
the whole routed fleet. A deep interface **validates its whole
contract** at the boundary: it should not start in a state where one of
its own services cannot be reached.

### The configuration-problems literature

The corpus's search for *"characterizing configuration problems
configuration errors are silent difficult to detect"* returns
Arshad, Rebecca J. Krause, and Sanjay Bagchi, *"Characterizing
Configuration Problems in Java EE Application Servers: An Empirical
Study"* — the standard reference for the failure mode this iteration
hit: a **configuration error that is silent until a request reaches the
unconfigured path**. The study's framing is that configuration problems
are disproportionately hard to diagnose because the system *starts
successfully*.

## 4. The technique chosen

**Make the gateway's configuration contract complete, and validate it at
the boundary.**

Concretely: the gateway knows its own service list. A service it can
route but has no backend for is a **misconfiguration that makes every
request to that service fail**. The gateway should therefore:

1. **Refuse to start** when a service it is expected to route has no
   backend — naming the exact YAML key to add, with the address to add
   it to. A gateway that cannot route `docs` cannot serve a document
   link, and every document link the corpus's agents hand to users is
   dead.
2. **Say so in the error**, the way the rest of this project's errors
   are written: the key, the value, and what to do.

This is Ousterhout's "define errors out of existence" applied to a
configuration: the error case (a request to an unrouted service) is
**eliminated** by refusing the misconfiguration up front, not handled
gracefully at request time.

### Why not the alternatives

| the alternative | why not |
|---|---|
| **Add the `docs` route to `two`'s config by hand** | fixes this host, leaves the class of defect. The next service, or the next host, fails the same way. The *contract* is still incomplete. |
| **Make the gateway's 502 message name the config key** | better, but still a runtime error for a configuration problem: the gateway is *running* in a state where half its surface is dead, and nothing says so until a user clicks. |
| **Register the docs service dynamically** (the registry path) | the right long-term design, and the registry already exists — but `two` has no `registry:` section, so the static `routes:` is the *only* path there. Changing the registry is a different, larger change. |
| **Have the MCP not mint remote links it cannot serve** | the MCP does not know the gateway's routes, and asking it to would make the link's availability depend on a second service's state. |

### The expected effect

- The gateway **refuses to start** with a message that names
  `cloud.gateway.routes.docs` and the URL to add — instead of serving
  `502 No backend configured for service: docs` to every user.
- The operator fixes the config, and **the remote document links work**:
  0/10 → 10/10.
- The LAN path is unaffected.

## 5. What this iteration does *not* do

- It does not add the `docs` route to `two`'s config. That is a
  **deployment** step (a config edit on a live host), done after the
  code that makes the gap loud is released.
- It does not change the link format, the signature, the TTL, or the
  docs service. Those work.
- It does not touch the **page-map gap** — 27.5 % of search hits carry
  no page, so their source link has no `#page=` anchor. That is a
  separate, larger problem (the olmocr backend emits one flat blob
  with no page separators, so the byte→page map cannot be recovered
  for 58.3 % of the corpus), and it is a *different* user-visible
  symptom from a dead link. It is logged as the next item.
