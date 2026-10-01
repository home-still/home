//! Request-rate limiting for the unauthenticated credential endpoints.
//!
//! Enrollment codes carry ~30 bits and OAuth authorization is gated by the same
//! codes, so unlimited guessing is the attack to bound. The limiter is one
//! token bucket per endpoint, deliberately NOT keyed by client address: behind
//! cloudflared every peer is loopback, and `cf-connecting-ip` is attacker
//! controlled whenever the listener is reachable directly, so no per-client key
//! is trustworthy. A global bucket caps the total guess rate regardless of who
//! is guessing; the cost is that a flood can briefly starve legitimate
//! enrollment, which is the safe side of that trade.

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
}

/// One bucket per guessable endpoint.
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

    fn for_request(&self, method: &Method, path: &str) -> Option<&RateLimiter> {
        if method != Method::POST {
            return None;
        }
        match path {
            "/cloud/enroll" => Some(&self.enroll),
            "/authorize" => Some(&self.authorize),
            "/token" => Some(&self.token),
            "/register" => Some(&self.register),
            _ => None,
        }
    }
}

/// Middleware: answer 429 once an endpoint's bucket is empty.
pub async fn limit(State(state): State<Arc<GatewayState>>, req: Request, next: Next) -> Response {
    if let Some(limiter) = state
        .rate_limits
        .for_request(req.method(), req.uri().path())
    {
        if let Err(retry_after) = limiter.check() {
            let mut resp = (StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response();
            if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            return resp;
        }
    }
    next.run(req).await
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
        assert!(limits
            .for_request(&Method::POST, "/registry/register")
            .is_none());
    }
}
