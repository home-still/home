use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::protocol::{ReadinessInfo, ServiceClient};

/// Generic server pool with readiness-based selection and round-robin tie-breaking.
pub struct ServicePool<C: ServiceClient> {
    clients: Vec<C>,
    next: AtomicUsize,
    /// Serializes `pick_server` calls. Without this, a burst of N
    /// concurrent handlers all probe readiness in parallel, all read
    /// the same pre-dispatch snapshot, and dog-pile onto whichever
    /// server happened to look best.
    pick_lock: tokio::sync::Mutex<()>,
    /// Client-side in-flight counter per server. Incremented when
    /// `pick_server` returns that server (and held by a
    /// [`PickGuard`] until the caller drops it after convert).
    /// Scribe-server's own `vlm_slots_available` lags: it only
    /// increments after the full multipart body is received, which
    /// for a 500-page book takes longer than the pick-to-dispatch
    /// gap. Tracking reservations client-side fixes the dog-pile
    /// without needing the server to respond instantly.
    reservations: Arc<Vec<AtomicUsize>>,
}

/// Returned by [`ServicePool::pick_server`]. Decrements the pool's
/// reservation counter for the chosen server when dropped. Hold this
/// for the duration of the dispatch (convert call).
pub struct PickGuard {
    reservations: Arc<Vec<AtomicUsize>>,
    idx: usize,
}

impl Drop for PickGuard {
    fn drop(&mut self) {
        self.reservations[self.idx].fetch_sub(1, Ordering::Relaxed);
    }
}

/// How long `pick_server` polls for a ready server before giving up.
/// Handlers PARK here when every VLM slot is held by an in-progress
/// convert. With a timeout too short for a book-sized convert (e.g.
/// 300+ pages × ~15 s/page), handlers NAK and JetStream redelivers —
/// wasting cycles because the book will still be running by the time
/// the redelivered message shows up. Set to slightly above the scribe
/// timeout ceiling (3600 s) so a handler only gives up when it's
/// genuinely clear no slot will ever open.
const PICK_READY_TIMEOUT: Duration = Duration::from_secs(3900);
/// Gap between readiness probe cycles while waiting.
const PICK_POLL_INTERVAL: Duration = Duration::from_millis(500);

impl<C: ServiceClient> ServicePool<C> {
    pub fn new(clients: Vec<C>) -> Self {
        let reservations: Vec<AtomicUsize> =
            (0..clients.len()).map(|_| AtomicUsize::new(0)).collect();
        Self {
            clients,
            next: AtomicUsize::new(0),
            pick_lock: tokio::sync::Mutex::new(()),
            reservations: Arc::new(reservations),
        }
    }

    /// Number of concurrent operations to allow (4 per server — matches
    /// scribe's default VLM slot count, so the watcher's dispatch ceiling
    /// equals the cluster's aggregate compute ceiling).
    pub fn concurrency(&self) -> usize {
        (self.clients.len() * 4).max(1)
    }

    /// Pick the least-loaded ready server with round-robin tie-breaking.
    /// When every probed server reports zero available slots, poll at
    /// [`PICK_POLL_INTERVAL`] until a slot frees up or
    /// [`PICK_READY_TIMEOUT`] elapses. The poll-wait keeps bursty
    /// event-bus deliveries from being dropped the moment the pool
    /// happens to be full — they briefly park here instead.
    pub async fn pick_server(&self) -> Result<(&C, PickGuard)> {
        let _pick_guard = self.pick_lock.lock().await;
        let deadline = Instant::now() + PICK_READY_TIMEOUT;
        let mut attempt: u32 = 0;
        loop {
            if let Some((c, idx)) = self.try_pick_once().await? {
                self.reservations[idx].fetch_add(1, Ordering::Relaxed);
                let guard = PickGuard {
                    reservations: Arc::clone(&self.reservations),
                    idx,
                };
                return Ok((c, guard));
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "no ready server after {}s of polling",
                    PICK_READY_TIMEOUT.as_secs()
                );
            }
            attempt += 1;
            if attempt == 1 {
                tracing::debug!(
                    interval_ms = PICK_POLL_INTERVAL.as_millis() as u64,
                    timeout_s = PICK_READY_TIMEOUT.as_secs(),
                    "pool saturated — polling for readiness"
                );
            }
            tokio::time::sleep(PICK_POLL_INTERVAL).await;
        }
    }

    async fn try_pick_once(&self) -> Result<Option<(&C, usize)>> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        for (c, r) in &results {
            if let Err(e) = r {
                tracing::warn!(
                    server = %c.url(),
                    error = %e,
                    "readiness probe failed; excluding from this dispatch"
                );
            }
        }

        // Effective available slots = server-reported available minus our
        // outstanding reservations. This handles the case where several
        // picks land before the server's in_flight counter catches up
        // with the most recent HTTP POSTs.
        let effective: Vec<Option<(usize, usize)>> = results
            .iter()
            .enumerate()
            .map(|(i, (_, r))| {
                let r = r.as_ref().ok()?;
                if !r.is_ready() {
                    return None;
                }
                let reserved = self.reservations[i].load(Ordering::Relaxed);
                let avail = r.available_slots().saturating_sub(reserved);
                if avail == 0 {
                    return None;
                }
                Some((i, avail))
            })
            .collect();

        let max_avail = effective
            .iter()
            .filter_map(|o| o.as_ref())
            .map(|(_, a)| *a)
            .max();

        let Some(max_avail) = max_avail else {
            return Ok(None);
        };

        let candidates: Vec<usize> = effective
            .iter()
            .filter_map(|o| {
                o.as_ref()
                    .and_then(|(i, a)| (*a == max_avail).then_some(*i))
            })
            .collect();

        if candidates.is_empty() {
            return Ok(None);
        }

        let rr = self.next.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let idx = candidates[rr];
        Ok(Some((&self.clients[idx], idx)))
    }

    /// Health check all servers. Returns (url, reachable) pairs.
    pub async fn check_all(&self) -> Vec<(String, bool)> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c.url().to_string(), c.health().await.is_ok()) })
            .collect();
        futures::future::join_all(futures).await
    }

    /// Get a reference to all clients.
    pub fn clients(&self) -> &[C] {
        &self.clients
    }
}
