//! Request-rate limiting for the unauthenticated credential endpoints.
//!
//! The limiter is one token bucket per endpoint, deliberately NOT keyed by
//! client address: behind cloudflared every peer is loopback, and
//! `cf-connecting-ip` is attacker controlled whenever the listener is
//! reachable directly, so no per-client key is trustworthy. Buckets are global
//! and charged according to what the endpoint can leak:
//!
//! * `/cloud/enroll`, `/authorize` — guessable (~30-bit codes). A token is
//!   reserved *before* the handler runs (concurrent guesses cannot overshoot)
//!   and refunded when the request succeeded, so only **failed** attempts use
//!   the budget and total guessing is bounded by the bucket. Validation errors
//!   that happen before the code is looked up charge too (harmless: they leak
//!   nothing). While the bucket is empty every request gets 429, valid or not.
//! * `/token` — nothing here is guessable: refresh tokens are HMAC-signed and
//!   authorization codes are ~165-bit single-use values bound to PKCE. The
//!   handler always runs; only **failed** grants are charged, and once the
//!   failure budget is spent a failure is answered with 429. A valid code
//!   exchange or refresh grant is never refused, so an anonymous flood cannot
//!   lock OAuth clients out of refreshing.
//! * `/register` — creates state (and evicts the oldest client when full), so
//!   every request is charged.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;

use crate::state::GatewayState;

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// A token bucket: `capacity` requests burst, refilled at `refill_per_sec`.
pub struct RateLimiter {
    bucket: Mutex<Bucket>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimiter {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                tokens: f64::from(capacity),
                last_refill: Instant::now(),
            }),
            capacity: f64::from(capacity),
            refill_per_sec,
        }
    }

    /// `per_minute` requests per minute, with a burst of the same size.
    pub fn per_minute(per_minute: u32) -> Self {
        Self::new(per_minute, f64::from(per_minute) / 60.0)
    }

    /// Take one token. On refusal returns the seconds until one is available.
    pub fn check(&self) -> Result<(), u64> {
        let mut bucket = self.bucket.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let wait = if self.refill_per_sec > 0.0 {
            ((1.0 - bucket.tokens) / self.refill_per_sec).ceil() as u64
        } else {
            u64::MAX
        };
        Err(wait.max(1))
    }

    /// Give back a token taken by [`Self::check`] for a request that turned
    /// out to be legitimate.
    pub fn refund(&self) {
        let mut bucket = self.bucket.lock();
        bucket.tokens = (bucket.tokens + 1.0).min(self.capacity);
    }
}

/// How an endpoint charges its bucket (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    /// Reserve before, refund on success: only failures are spent; empty ⇒ 429 for all.
    ReserveRefundOnSuccess,
    /// Always run; charge failures; a failure past the budget becomes 429.
    ChargeFailuresOnly,
    /// Charge every request.
    ChargeAlways,
}

/// One bucket per credential endpoint.
pub struct RateLimits {
    enroll: RateLimiter,
    authorize: RateLimiter,
    token: RateLimiter,
    register: RateLimiter,
}

impl RateLimits {
    pub fn per_minute(per_minute: u32) -> Self {
        Self {
            enroll: RateLimiter::per_minute(per_minute),
            authorize: RateLimiter::per_minute(per_minute),
            token: RateLimiter::per_minute(per_minute),
            register: RateLimiter::per_minute(per_minute),
        }
    }

    fn for_request(&self, method: &Method, path: &str) -> Option<(&RateLimiter, Policy)> {
        if method != Method::POST {
            return None;
        }
        match path {
            "/cloud/enroll" => Some((&self.enroll, Policy::ReserveRefundOnSuccess)),
            "/authorize" => Some((&self.authorize, Policy::ReserveRefundOnSuccess)),
            "/token" => Some((&self.token, Policy::ChargeFailuresOnly)),
            "/register" => Some((&self.register, Policy::ChargeAlways)),
            _ => None,
        }
    }
}

fn too_many(retry_after: u64) -> Response {
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response();
    if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

/// A refused attempt: any 4xx/5xx from the handler.
fn is_failure(resp: &Response) -> bool {
    resp.status().is_client_error() || resp.status().is_server_error()
}

/// Middleware applying each credential endpoint's [`Policy`].
pub async fn limit(State(state): State<Arc<GatewayState>>, req: Request, next: Next) -> Response {
    let Some((limiter, policy)) = state
        .rate_limits
        .for_request(req.method(), req.uri().path())
    else {
        return next.run(req).await;
    };

    match policy {
        Policy::ChargeAlways => match limiter.check() {
            Ok(()) => next.run(req).await,
            Err(wait) => too_many(wait),
        },
        Policy::ReserveRefundOnSuccess => {
            if let Err(wait) = limiter.check() {
                return too_many(wait);
            }
            let resp = next.run(req).await;
            if !is_failure(&resp) {
                limiter.refund();
            }
            resp
        }
        Policy::ChargeFailuresOnly => {
            let resp = next.run(req).await;
            if is_failure(&resp) {
                if let Err(wait) = limiter.check() {
                    return too_many(wait);
                }
            }
            resp
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_allowed_then_refused() {
        let limiter = RateLimiter::new(3, 0.0);
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err());
    }

    #[test]
    fn refusal_reports_a_nonzero_wait() {
        let limiter = RateLimiter::per_minute(1);
        assert!(limiter.check().is_ok());
        let wait = limiter.check().unwrap_err();
        assert!((1..=60).contains(&wait), "{wait}");
    }

    #[test]
    fn refund_gives_a_token_back_but_never_beyond_capacity() {
        let limiter = RateLimiter::new(2, 0.0);
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err());
        limiter.refund();
        assert!(limiter.check().is_ok());
        limiter.refund();
        limiter.refund();
        limiter.refund();
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err(), "capacity is a hard ceiling");
    }

    #[test]
    fn tokens_refill_with_time() {
        let limiter = RateLimiter::new(1, 1000.0);
        assert!(limiter.check().is_ok());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(limiter.check().is_ok());
    }

    #[test]
    fn only_post_to_the_credential_endpoints_is_limited() {
        let limits = RateLimits::per_minute(5);
        for path in ["/cloud/enroll", "/authorize", "/token", "/register"] {
            assert!(limits.for_request(&Method::POST, path).is_some(), "{path}");
            assert!(limits.for_request(&Method::GET, path).is_none(), "{path}");
        }
        assert!(limits.for_request(&Method::POST, "/mcp").is_none());
    }
}
