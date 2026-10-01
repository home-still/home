use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::protocol::{ReadinessInfo, ServiceClient};

/// Generic server pool with readiness-based selection and round-robin tie-breaking.
pub struct ServicePool<C: ServiceClient> {
    clients: Vec<C>,
    next: AtomicUsize,
    /// Serializes the SELECT→CLAIM step inside `try_pick_once`. Without
    /// it, a burst of N concurrent handlers that probed the same snapshot
    /// would all read the same reservation count and dog-pile onto
    /// whichever server looked best. Held only across that CPU-only step
    /// — never across a readiness probe (one stalled host would serialize
    /// every picker behind its timeout) and never across the poll-sleep (a
    /// single parked caller would serialize every other picker for up to
    /// the full [`PICK_READY_TIMEOUT`]).
    pick_lock: tokio::sync::Mutex<()>,
    /// See [`PICK_READY_TIMEOUT`] / [`PICK_POLL_INTERVAL`]. Stored per
    /// pool so tests can shrink them; production always uses the consts.
    ready_timeout: Duration,
    poll_interval: Duration,
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

/// `pick_server` error when every host in the pool answered its readiness
/// probe and refused work at its admission gate (scribe's
/// `backend_unavailable` VRAM gate). The gate opens when another GPU
/// tenant leaves, not within a dispatch, so the pick fails at once instead
/// of parking for [`PICK_READY_TIMEOUT`] while holding the caller's
/// permits. Callers `downcast_ref` it to tell a closed tier from a
/// timed-out one.
#[derive(Debug)]
pub struct NoAdmittingHost;

impl std::fmt::Display for NoAdmittingHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("every host in the pool is refusing work at its admission gate")
    }
}

impl std::error::Error for NoAdmittingHost {}

/// Outcome of one probe→claim cycle in [`ServicePool::pick_server`].
#[derive(Debug)]
enum Probe<'a, C> {
    Picked(&'a C, usize),
    /// A host may take work once a slot frees or it wakes up (busy, or
    /// unreachable — a sleeping laptop). Keep polling.
    Wait,
    /// Every host answered and every one refused at its admission gate.
    Gated,
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
            ready_timeout: PICK_READY_TIMEOUT,
            poll_interval: PICK_POLL_INTERVAL,
        }
    }

    /// Shrink the poll-wait timing for tests. Not reachable in
    /// production builds — the consts are the one configuration.
    #[cfg(test)]
    fn with_timing(mut self, ready_timeout: Duration, poll_interval: Duration) -> Self {
        self.ready_timeout = ready_timeout;
        self.poll_interval = poll_interval;
        self
    }

    /// Number of concurrent operations to allow (4 per server — matches
    /// scribe's default VLM slot count, so the watcher's dispatch ceiling
    /// equals the cluster's aggregate compute ceiling).
    pub fn concurrency(&self) -> usize {
        (self.clients.len() * 4).max(1)
    }

    /// Probe each server's `/readiness` once and sum advertised slot
    /// capacity. Use this at consumer startup to size the in-flight
    /// semaphore correctly for heterogeneous fleets where
    /// `clients.len() * 4` undercounts capacity (e.g. an RTX 3090 host
    /// advertising 12 slots paired with two 6-slot Apple Silicon hosts —
    /// real ceiling is 24, not 12). Falls back to [`Self::concurrency`]
    /// when every probe fails so callers never get a worse-than-baseline
    /// cap.
    pub async fn probed_concurrency(&self) -> usize {
        let futs = self.clients.iter().map(|c| c.readiness());
        let results = futures::future::join_all(futs).await;
        let summed: usize = results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|info| info.total_slots())
            .sum();
        if summed == 0 {
            self.concurrency()
        } else {
            summed
        }
    }

    /// Pick the least-loaded ready server with round-robin tie-breaking.
    /// When every probed server reports zero available slots, poll at
    /// [`PICK_POLL_INTERVAL`] until a slot frees up or
    /// [`PICK_READY_TIMEOUT`] elapses. The poll-wait keeps bursty
    /// event-bus deliveries from being dropped the moment the pool
    /// happens to be full — they briefly park here instead. When every
    /// server refuses at its admission gate, fail at once with
    /// [`NoAdmittingHost`] instead.
    pub async fn pick_server(&self) -> Result<(&C, PickGuard)> {
        let deadline = Instant::now() + self.ready_timeout;
        let mut attempt: u32 = 0;
        loop {
            let log_failures = attempt == 0 || attempt.is_multiple_of(120);
            match self.try_pick_once(log_failures).await? {
                Probe::Picked(c, idx) => {
                    let guard = PickGuard {
                        reservations: Arc::clone(&self.reservations),
                        idx,
                    };
                    return Ok((c, guard));
                }
                Probe::Gated => return Err(NoAdmittingHost.into()),
                Probe::Wait => {}
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "no ready server after {}s of polling",
                    self.ready_timeout.as_secs()
                );
            }
            attempt += 1;
            if attempt == 1 {
                tracing::debug!(
                    interval_ms = self.poll_interval.as_millis() as u64,
                    timeout_s = self.ready_timeout.as_secs(),
                    "pool saturated — polling for readiness"
                );
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// One probe→claim cycle.
    ///
    /// The `/readiness` probes run with no lock held, so a host that takes
    /// seconds to time out (a sleeping laptop) delays only the pickers that
    /// are themselves probing it, never the claim of a ready host by another
    /// picker. `pick_lock` guards only the selection and the reservation
    /// that claims the slot, which is pure CPU: claiming against the live
    /// `reservations` is what keeps a burst of pickers that probed the same
    /// snapshot from dog-piling onto whichever server looked best.
    ///
    /// `Picked` means the reservation is already taken; the caller owns
    /// releasing it (via [`PickGuard`]).
    ///
    /// `log_failures` throttles the unreachable-server warning. A parked
    /// handler re-probes every [`PICK_POLL_INTERVAL`]; logging every
    /// failed probe turned one sleeping laptop into a continuous
    /// several-lines-per-second journal flood. The caller passes true on
    /// the first cycle and roughly once per minute after.
    async fn try_pick_once(&self, log_failures: bool) -> Result<Probe<'_, C>> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        // Only a positive refusal from EVERY host closes the pool. An
        // unreachable host is not proof of anything (it may be a laptop
        // that wakes up), so any Err keeps the caller polling.
        if !results.is_empty()
            && results
                .iter()
                .all(|(_, r)| matches!(r, Ok(info) if !info.admits_work()))
        {
            return Ok(Probe::Gated);
        }

        if log_failures {
            for (c, r) in &results {
                if let Err(e) = r {
                    tracing::warn!(
                        server = %c.url(),
                        error = %e,
                        "readiness probe failed; excluding from this dispatch"
                    );
                }
            }
        }

        let _claim = self.pick_lock.lock().await;

        // Effective available slots = server-reported available minus our
        // outstanding reservations. This handles the case where several
        // picks land before the server's in_flight counter catches up
        // with the most recent HTTP POSTs.
        let effective: Vec<Option<(usize, usize)>> = results
            .iter()
            .enumerate()
            .map(|(i, (_, r))| {
                let r = r.as_ref().ok()?;
                // Deliberately NOT gating on `r.is_ready()`. Scribe servers
                // report `ready: false` when Ollama has unloaded the model
                // (keep_alive expiry), which creates a self-reinforcing
                // starvation loop: the pool never dispatches → Ollama
                // never warms → ready never flips true. Use our own
                // reservation accounting + the server's slot count as the
                // sole eligibility signal; if the picked server turns
                // out to be cold, it pays the reload tax on the actual
                // request and unsticks itself for future picks. With
                // OLLAMA_KEEP_ALIVE set high this path is only hit once
                // per boot.
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
            return Ok(Probe::Wait);
        };

        let candidates: Vec<usize> = effective
            .iter()
            .filter_map(|o| {
                o.as_ref()
                    .and_then(|(i, a)| (*a == max_avail).then_some(*i))
            })
            .collect();

        if candidates.is_empty() {
            return Ok(Probe::Wait);
        }

        let rr = self.next.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let idx = candidates[rr];
        self.reservations[idx].fetch_add(1, Ordering::Relaxed);
        Ok(Probe::Picked(&self.clients[idx], idx))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::protocol::{ReadinessInfo, ServiceClient};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    #[derive(serde::Deserialize, Clone)]
    struct Health {
        _ok: Option<bool>,
    }

    #[derive(serde::Deserialize, Clone)]
    struct Readiness {
        ready: bool,
        avail: usize,
        admits: bool,
    }

    impl ReadinessInfo for Readiness {
        fn is_ready(&self) -> bool {
            self.ready
        }
        fn available_slots(&self) -> usize {
            self.avail
        }
        fn admits_work(&self) -> bool {
            self.admits
        }
    }

    #[derive(Debug)]
    struct MockClient {
        url: String,
        ready: Arc<AtomicBool>,
        avail: Arc<AtomicUsize>,
        /// Answers readiness but refuses at its admission gate.
        gated: Arc<AtomicBool>,
        /// Readiness probe fails (host asleep / unreachable).
        down: Arc<AtomicBool>,
        /// Readiness probe takes this long to answer (host half-asleep).
        stall: Duration,
    }

    #[async_trait]
    impl ServiceClient for MockClient {
        type Health = Health;
        type Readiness = Readiness;

        fn url(&self) -> &str {
            &self.url
        }
        async fn health(&self) -> Result<Health> {
            Ok(Health { _ok: Some(true) })
        }
        async fn readiness(&self) -> Result<Readiness> {
            if !self.stall.is_zero() {
                tokio::time::sleep(self.stall).await;
            }
            if self.down.load(AtomicOrdering::Relaxed) {
                anyhow::bail!("connection refused");
            }
            Ok(Readiness {
                ready: self.ready.load(AtomicOrdering::Relaxed),
                avail: self.avail.load(AtomicOrdering::Relaxed),
                admits: !self.gated.load(AtomicOrdering::Relaxed),
            })
        }
    }

    fn mk(url: &str, ready: bool, avail: usize) -> MockClient {
        MockClient {
            url: url.into(),
            ready: Arc::new(AtomicBool::new(ready)),
            avail: Arc::new(AtomicUsize::new(avail)),
            gated: Arc::new(AtomicBool::new(false)),
            down: Arc::new(AtomicBool::new(false)),
            stall: Duration::ZERO,
        }
    }

    fn gated(url: &str) -> MockClient {
        let c = mk(url, false, 0);
        c.gated.store(true, AtomicOrdering::Relaxed);
        c
    }

    fn down(url: &str) -> MockClient {
        let c = mk(url, false, 0);
        c.down.store(true, AtomicOrdering::Relaxed);
        c
    }

    fn stalled(url: &str, avail: usize, stall: Duration) -> MockClient {
        let mut c = mk(url, true, avail);
        c.stall = stall;
        c
    }

    /// RA-59: `/readiness` probes run outside `pick_lock`. With the lock
    /// held across them, N pickers each waited out the stalled host's probe
    /// one after another (N × stall); now they probe, and wait, together.
    #[tokio::test]
    async fn pickers_do_not_serialize_behind_a_stalled_host() {
        let stall = Duration::from_millis(400);
        let pool = Arc::new(ServicePool::new(vec![
            stalled("http://asleep:7433", 4, stall),
            mk("http://ok:7433", true, 8),
        ]));

        let started = Instant::now();
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let pool = Arc::clone(&pool);
                tokio::spawn(async move {
                    let (client, guard) = pool.pick_server().await.unwrap();
                    let url = client.url().to_string();
                    (url, guard)
                })
            })
            .collect();
        let mut guards = Vec::new();
        for t in tasks {
            guards.push(t.await.unwrap());
        }
        let elapsed = started.elapsed();
        assert_eq!(guards.len(), 6);
        assert!(
            elapsed < stall * 3,
            "6 pickers behind one {stall:?} probe must overlap (~{stall:?}), \
             not serialize (~{:?}); took {elapsed:?}",
            stall * 6
        );
    }

    /// Moving the probes out of the lock must not reopen the dog-pile: the
    /// claim is still atomic against live reservations, so a burst that all
    /// probed `avail = 2` claims exactly 2 slots.
    #[tokio::test]
    async fn concurrent_pickers_never_claim_more_than_the_free_slots() {
        let pool = Arc::new(
            ServicePool::new(vec![mk("http://one:7433", true, 2)])
                .with_timing(Duration::from_millis(200), Duration::from_millis(20)),
        );
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let pool = Arc::clone(&pool);
                tokio::spawn(async move {
                    match pool.pick_server().await {
                        Ok((_, guard)) => {
                            // Hold the slot past every other picker's deadline.
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            drop(guard);
                            true
                        }
                        Err(_) => false,
                    }
                })
            })
            .collect();
        let mut claimed = 0;
        for t in tasks {
            if t.await.unwrap() {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 2, "only the 2 advertised slots may be claimed");
    }

    #[tokio::test]
    async fn pick_server_includes_cold_server_reporting_not_ready() {
        // rc.304 invariant: a server reporting `ready: false` but with
        // free slots must still be dispatch-eligible. Ollama's keep_alive
        // unload flips `ready` to false even though the VLM slots are
        // free — the pre-rc.304 pool starved such a server forever.
        let cold_only = ServicePool::new(vec![mk("http://cold:7433", false, 4)]);
        let (client, _guard) = cold_only
            .pick_server()
            .await
            .expect("cold server must be dispatch-eligible");
        assert_eq!(client.url(), "http://cold:7433");
    }

    #[tokio::test]
    async fn pick_server_excludes_zero_slot_servers() {
        // A server with no free slots (reservation + in-flight = capacity)
        // is still correctly excluded. Reliability: we never send a
        // request to a scribe that definitely can't take it.
        let all_full = ServicePool::new(vec![
            mk("http://a:7433", true, 0),
            mk("http://b:7433", false, 0),
        ]);
        // Use try_pick_once directly so the test doesn't wait
        // PICK_READY_TIMEOUT seconds for availability.
        let res = all_full.try_pick_once(true).await.unwrap();
        assert!(
            matches!(res, Probe::Wait),
            "all-full pool must keep waiting, got {res:?}"
        );
    }

    #[tokio::test]
    async fn pick_server_uses_cold_when_warm_is_saturated() {
        // Two servers: a "warm" one at its ceiling (ready:true but zero
        // slots), a "cold" one with spare slots (ready:false). Before
        // rc.304, the pool would loop forever polling the warm server;
        // after rc.304, the cold server is picked immediately.
        let pool = ServicePool::new(vec![
            mk("http://warm:7433", true, 0),
            mk("http://cold:7433", false, 3),
        ]);
        let (client, _guard) = pool.pick_server().await.unwrap();
        assert_eq!(client.url(), "http://cold:7433");
    }

    #[tokio::test]
    async fn saturated_pool_times_out_pickers_concurrently_not_serially() {
        // pick_lock must be scoped to one probe→claim cycle, not held
        // across the poll-sleep. Held-across-sleep, N concurrent callers
        // on a permanently saturated pool time out SERIALLY (each only
        // starts after the previous gives up → N × timeout); correctly
        // scoped, they all poll in parallel and give up together.
        let pool = Arc::new(
            ServicePool::new(vec![mk("http://full:7433", true, 0)])
                .with_timing(Duration::from_millis(300), Duration::from_millis(50)),
        );

        let started = Instant::now();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let pool = Arc::clone(&pool);
                tokio::spawn(async move { pool.pick_server().await.is_err() })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap(), "saturated pool must time out the pick");
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(900),
            "4 pickers on a 300ms timeout must fail concurrently (~300ms), \
             not serially (~1200ms); took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn gated_pool_fails_the_pick_at_once() {
        // big's scribe answers `backend_unavailable` while another GPU
        // tenant holds the card. Parking for the full ready timeout held
        // the tier permit and wedged every watch-events slot.
        let pool = ServicePool::new(vec![gated("http://big:7435")])
            .with_timing(Duration::from_secs(60), Duration::from_millis(50));
        let started = Instant::now();
        let err = pool
            .pick_server()
            .await
            .err()
            .expect("gated pool must fail");
        assert!(err.is::<NoAdmittingHost>(), "wrong error: {err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn one_gated_host_does_not_close_the_pool() {
        // Only a refusal from EVERY host closes the pool; a gated host
        // beside a busy-but-admitting one means wait for the busy one.
        let pool = ServicePool::new(vec![
            gated("http://big:7435"),
            mk("http://bmb:7433", true, 0),
        ]);
        let res = pool.try_pick_once(false).await.unwrap();
        assert!(matches!(res, Probe::Wait), "got {res:?}");
    }

    #[tokio::test]
    async fn unreachable_host_is_polled_not_failed_fast() {
        // A sleeping laptop is not a closed gate: the pick keeps polling
        // for the full timeout so the event is not NAKed through its
        // JetStream delivery budget in minutes.
        let pool = ServicePool::new(vec![down("http://bmb:7433"), gated("http://big:7435")])
            .with_timing(Duration::from_millis(300), Duration::from_millis(50));
        let started = Instant::now();
        let err = pool.pick_server().await.err().expect("must time out");
        assert!(
            !err.is::<NoAdmittingHost>(),
            "unreachable host closed the pool"
        );
        assert!(started.elapsed() >= Duration::from_millis(300));
    }
}
