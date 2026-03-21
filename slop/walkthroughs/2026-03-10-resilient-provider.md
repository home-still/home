# Walkthrough: Resilient Provider Wrapper

**Date:** 2026-03-10
**Status:** In Progress
**Checkpoint:** fbeac307b5257c4a3a0e241a617ea89bca1c7e8f

## Goal

Wire the existing resilience infrastructure (rate limiter, circuit breaker, retry) into all providers via a transparent decorator pattern.

## Acceptance Criteria

- [ ] `ResilientProvider` wraps any `Box<dyn PaperProvider>` and applies rate limiting, circuit breaker, and retry
- [ ] Rate limiting is per-provider (arXiv: 1 req/3 sec), configured via provider config
- [ ] Rapid searches wait instead of getting 503'd
- [ ] 503 responses are mapped to `PaperError::RateLimited` and retried automatically
- [ ] Empty `rate_limit.rs` placeholder is cleaned up
- [ ] `cargo clippy --workspace -- -D warnings` passes

## Technical Approach

### Architecture

```
CLI (commands/paper.rs)
  └─ make_provider()
       └─ ResilientProvider<CB>         ← NEW wrapper
            ├─ ProviderRateLimiter      ← existing, simplified
            ├─ circuit_breaker (CB)     ← existing
            ├─ retry_with_backoff()     ← existing
            └─ ArxivProvider            ← unchanged
```

### Key Decisions

- **Decorator pattern**: Providers stay focused on HTTP/parsing. Resilience is applied in `make_provider()` transparently.
- **Per-provider rate config**: `rate_limit_interval_ms` on each provider config (not global), because different APIs have different limits.
- **Single limiter per wrapper**: Dropped the `HashMap<String, Limiter>` design — each `ResilientProvider` owns one limiter for its one inner provider.
- **Rate limit outside retry loop**: `acquire()` called once before retries, not on each attempt.

### Existing Code to Reuse

- `crates/paper-core/src/resilience/rate_limiter.rs` — `ProviderRateLimiter` (simplify)
- `crates/paper-core/src/resilience/circuit_breaker.rs` — `new_circuit_breaker()` (use as-is)
- `crates/paper-core/src/resilience/retry.rs` — `retry_with_backoff()` (one-line change)
- `crates/paper-core/src/resilience/config.rs` — `ResilienceConfig` (remove `rate_limit_rps`)
- `crates/paper-core/src/error.rs` — `PaperError::RateLimited`, `ErrorCategory` (use as-is)

### Files to Create/Modify

- `crates/paper-core/src/rate_limit.rs`: **delete** (empty placeholder)
- `crates/paper-core/src/lib.rs`: remove `pub mod rate_limit;`
- `crates/paper-core/src/config.rs`: add `rate_limit_interval_ms` to `ArxivConfig`
- `crates/paper-core/src/resilience/config.rs`: remove `rate_limit_rps`
- `crates/paper-core/src/resilience/rate_limiter.rs`: simplify to single-limiter with `Duration`
- `crates/paper-core/src/resilience/retry.rs`: also retry `RateLimited` errors
- `crates/paper-core/src/providers/resilient.rs`: **create** `ResilientProvider`
- `crates/paper-core/src/providers/mod.rs`: add `pub mod resilient;`
- `crates/paper-core/src/providers/arxiv.rs`: map 503 → `PaperError::RateLimited`
- `crates/paper/src/commands/paper.rs`: wrap in `ResilientProvider` in `make_provider()`

## Steps

### Step 1: Clean up empty `rate_limit.rs`
**Status:** [ ] Not started

Delete `crates/paper-core/src/rate_limit.rs` and remove `pub mod rate_limit;` from `lib.rs`.

### Step 2: Per-provider rate limit config
**Status:** [ ] Not started

Add `rate_limit_interval_ms: u64` to `ArxivConfig` (default 3000). Remove `rate_limit_rps` from `ResilienceConfig`.

### Step 3: Simplify `ProviderRateLimiter`
**Status:** [ ] Not started

Rewrite to take `Duration`, use `Quota::with_period()`, drop HashMap.

### Step 4: Retry `RateLimited` errors
**Status:** [ ] Not started

One-line change to `.when()` predicate in `retry.rs`.

### Step 5: Create `ResilientProvider`
**Status:** [ ] Not started

New file implementing `PaperProvider` with rate limit → circuit breaker → retry flow.

### Step 6: Map 503 to `RateLimited` in ArxivProvider
**Status:** [ ] Not started

Check response status before success check in `arxiv.rs`.

### Step 7: Wire into `make_provider()`
**Status:** [ ] Not started

Wrap inner provider with `ResilientProvider` in the factory function.

## Known Dragons

- **`failsafe::CircuitBreaker` generics**: `ResilientProvider` must be generic over `CB`. The concrete type is erased via `Box<dyn PaperProvider>` in `make_provider()`.
- **Borrow checker in retry closure**: Closure captures `&self.inner`. If async borrows complain, may need `Arc<dyn PaperProvider>` instead of `Box`.
- **`governor::Quota::with_period`**: Takes `Duration`, returns `Option<Quota>` — must unwrap (interval is always > 0 from config).

---
*Plan created: 2026-03-10*
*User implementation started: 2026-03-10*
