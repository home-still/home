//! Single source of truth for NVIDIA GPU state.
//!
//! Every home-still GPU consumer (scribe's VLM admission gate, distill's
//! embedder load gate, `/health` reporting) reads the card through this
//! module. One `nvidia-smi` shell-out per query, `None`/empty on every
//! failure so non-NVIDIA hosts (Apple Silicon pool members, Pis) keep a
//! clean response instead of a fabricated zero.
//!
//! Every shell-out is bounded: it is killed after a timeout and further
//! calls fail fast for a cooldown, because a wedged driver — the very
//! failure these queries diagnose — makes `nvidia-smi` hang forever. The
//! plain functions BLOCK for up to that timeout and are for sync startup
//! code; async request handlers use the `*_async` accessors, which run the
//! shell-out on a blocking thread and cache the answer for a few seconds.
//!
//! Whole-card `memory_used_mb` is NOT a residency signal under
//! co-tenancy: on a contended card it reflects other processes'
//! allocations. Use [`self_vram_mb`] when the question is "did *my*
//! process get GPU memory?" and [`free_vram_mb`] when the question is
//! "can I still fit?".

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub utilization_pct: Option<f32>,
    pub memory_used_mb: Option<u64>,
    pub memory_free_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
}

/// One CUDA process resident on the card, as reported by
/// `nvidia-smi --query-compute-apps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeApp {
    pub pid: u32,
    pub used_mb: u64,
    pub name: String,
}

/// How long one `nvidia-smi` invocation may run. A healthy call returns in
/// well under a second; a wedged driver (the failure this module exists to
/// diagnose) never returns.
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// After a timeout, further invocations fail at once for this long instead
/// of piling one more stuck child (and reaper thread) per call onto an
/// already wedged driver.
const NVIDIA_SMI_COOLDOWN: Duration = Duration::from_secs(30);

/// Bounded external-command runner with a post-timeout cooldown.
struct BoundedCmd {
    wedged_until: Mutex<Option<Instant>>,
}

impl BoundedCmd {
    const fn new() -> Self {
        Self {
            wedged_until: Mutex::new(None),
        }
    }

    /// Run `program args`, returning its stdout. `None` when the binary is
    /// absent, exits non-zero, outlives `timeout`, or a previous call timed
    /// out less than `cooldown` ago. A timed-out child is killed and
    /// reaped on a detached thread: SIGKILL does not land on a process
    /// stuck in an uninterruptible driver call, so waiting for it here
    /// would reintroduce the hang.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
        cooldown: Duration,
    ) -> Option<String> {
        if let Some(until) = *self.wedged_until.lock().unwrap_or_else(|e| e.into_inner()) {
            if Instant::now() < until {
                return None;
            }
        }
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    let mut out = String::new();
                    child.stdout.take()?.read_to_string(&mut out).ok()?;
                    return Some(out);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    *self.wedged_until.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now() + cooldown);
                    let _ = child.kill();
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return None;
                }
                Err(_) => return None,
            }
        }
    }
}

static NVIDIA_SMI: BoundedCmd = BoundedCmd::new();

/// Run `nvidia-smi` with the given query args, returning stdout.
/// `None` when the binary is absent, exits non-zero or times out.
/// BLOCKING (up to [`NVIDIA_SMI_TIMEOUT`]): async code must use the
/// `*_async` accessors, which run this on a blocking thread.
fn nvidia_smi(args: &[&str]) -> Option<String> {
    NVIDIA_SMI.run("nvidia-smi", args, NVIDIA_SMI_TIMEOUT, NVIDIA_SMI_COOLDOWN)
}

/// Shell out to nvidia-smi once and parse the first GPU's name,
/// utilization %, and memory used / free / total in MiB. All fields
/// `None` when nvidia-smi is absent or fails.
pub fn query_gpu_info() -> GpuInfo {
    let Some(stdout) = nvidia_smi(&[
        "--query-gpu=name,utilization.gpu,memory.used,memory.free,memory.total",
        "--format=csv,noheader,nounits",
    ]) else {
        return GpuInfo::default();
    };
    parse_gpu_info(&stdout)
}

fn parse_gpu_info(stdout: &str) -> GpuInfo {
    let line = stdout.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return GpuInfo::default();
    }
    let mut parts = line.split(',').map(str::trim);
    GpuInfo {
        name: parts.next().filter(|s| !s.is_empty()).map(String::from),
        utilization_pct: parts.next().and_then(|s| s.parse::<f32>().ok()),
        memory_used_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
        memory_free_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
        memory_total_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
    }
}

/// Free VRAM in MiB on the first GPU. `None` on hosts without a working
/// `nvidia-smi` — callers MUST treat that as "no gate to apply", never
/// as zero free.
pub fn free_vram_mb() -> Option<u64> {
    query_gpu_info().memory_free_mb
}

/// VRAM in MiB attributed to *this* process. `None` when nvidia-smi is
/// unavailable; `Some(0)` when the card is visible but this process
/// holds no allocation (the CPU-fallback signal).
pub fn self_vram_mb() -> Option<u64> {
    let apps = compute_apps_raw()?;
    Some(vram_of_pid(&apps, std::process::id()))
}

/// Sum of `pid`'s own allocations. Per-process attribution is the whole
/// point: whole-card `memory.used` also counts every other tenant.
fn vram_of_pid(apps: &[ComputeApp], pid: u32) -> u64 {
    apps.iter()
        .filter(|a| a.pid == pid)
        .map(|a| a.used_mb)
        .sum()
}

/// Every CUDA process resident on the card. Empty when nvidia-smi is
/// unavailable or nothing is resident.
pub fn compute_apps() -> Vec<ComputeApp> {
    compute_apps_raw().unwrap_or_default()
}

fn compute_apps_raw() -> Option<Vec<ComputeApp>> {
    let stdout = nvidia_smi(&[
        "--query-compute-apps=pid,used_gpu_memory,process_name",
        "--format=csv,noheader,nounits",
    ])?;
    Some(parse_compute_apps(&stdout))
}

fn parse_compute_apps(stdout: &str) -> Vec<ComputeApp> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.split(',').map(str::trim);
            let pid = parts.next()?.parse::<u32>().ok()?;
            let used_mb = parts.next()?.parse::<u64>().ok()?;
            // process_name may itself contain commas; keep the remainder.
            let rest: Vec<&str> = parts.collect();
            let full = rest.join(",");
            let name = full.rsplit('/').next().unwrap_or(&full).trim().to_string();
            Some(ComputeApp { pid, used_mb, name })
        })
        .collect()
}

/// Operator-facing one-liner naming who is holding the card, biggest
/// first: `"pid=613301 7394MB llama-server; pid=588655 8188MB ollama"`.
/// `"none"` when nothing is resident or the card is not visible.
pub fn compute_apps_summary() -> String {
    let mut apps = compute_apps();
    if apps.is_empty() {
        return "none".to_string();
    }
    apps.sort_by_key(|a| std::cmp::Reverse(a.used_mb));
    apps.iter()
        .map(|a| format!("pid={} {}MB {}", a.pid, a.used_mb, a.name))
        .collect::<Vec<_>>()
        .join("; ")
}

/// How long the async accessors reuse one `nvidia-smi` answer. Health
/// endpoints and admission probes call these per request; the card state
/// they report does not change meaningfully inside a couple of seconds.
#[cfg(feature = "gpu-async")]
const GPU_SNAPSHOT_TTL: Duration = Duration::from_secs(2);

/// One cached value, refreshed on a blocking thread by at most one caller
/// at a time (everyone else waits on the refresh instead of starting
/// another `nvidia-smi`).
#[cfg(feature = "gpu-async")]
struct TtlCache<T> {
    slot: tokio::sync::Mutex<Option<(Instant, T)>>,
}

#[cfg(feature = "gpu-async")]
impl<T: Clone + Default + Send + 'static> TtlCache<T> {
    const fn new() -> Self {
        Self {
            slot: tokio::sync::Mutex::const_new(None),
        }
    }

    async fn get(&self, ttl: Duration, produce: impl FnOnce() -> T + Send + 'static) -> T {
        let mut slot = self.slot.lock().await;
        if let Some((at, value)) = slot.as_ref() {
            if at.elapsed() < ttl {
                return value.clone();
            }
        }
        let fresh = match tokio::task::spawn_blocking(produce).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "gpu probe task failed");
                T::default()
            }
        };
        *slot = Some((Instant::now(), fresh.clone()));
        fresh
    }
}

#[cfg(feature = "gpu-async")]
static INFO_CACHE: TtlCache<GpuInfo> = TtlCache::new();
#[cfg(feature = "gpu-async")]
static APPS_CACHE: TtlCache<String> = TtlCache::new();

/// Async, cached [`query_gpu_info`] for request handlers: the shell-out
/// runs on a blocking thread (never on a runtime worker) and its answer is
/// reused for a couple of seconds. Same all-`None` result when
/// `nvidia-smi` is absent, wedged or failing.
#[cfg(feature = "gpu-async")]
pub async fn query_gpu_info_async() -> GpuInfo {
    INFO_CACHE.get(GPU_SNAPSHOT_TTL, query_gpu_info).await
}

/// Async, cached [`free_vram_mb`]; `None` means "no gate to apply".
#[cfg(feature = "gpu-async")]
pub async fn free_vram_mb_async() -> Option<u64> {
    query_gpu_info_async().await.memory_free_mb
}

/// Async, cached [`compute_apps_summary`].
#[cfg(feature = "gpu-async")]
pub async fn compute_apps_summary_async() -> String {
    APPS_CACHE.get(GPU_SNAPSHOT_TTL, compute_apps_summary).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_gpu_query_line() {
        let info = parse_gpu_info("NVIDIA GeForce RTX 3090, 100, 23097, 1025, 24576\n");
        assert_eq!(info.name.as_deref(), Some("NVIDIA GeForce RTX 3090"));
        assert_eq!(info.utilization_pct, Some(100.0));
        assert_eq!(info.memory_used_mb, Some(23097));
        assert_eq!(info.memory_free_mb, Some(1025));
        assert_eq!(info.memory_total_mb, Some(24576));
    }

    #[test]
    fn empty_output_yields_all_none() {
        assert_eq!(parse_gpu_info("\n"), GpuInfo::default());
    }

    #[test]
    fn parses_compute_apps_and_basenames_process_path() {
        let apps = parse_compute_apps(
            "613301, 7394, /usr/lib/ollama/llama-server\n588655, 8188, /home/x/.local/llama.cpp/cuda-ece963/llama-server\n",
        );
        assert_eq!(
            apps,
            vec![
                ComputeApp {
                    pid: 613301,
                    used_mb: 7394,
                    name: "llama-server".into()
                },
                ComputeApp {
                    pid: 588655,
                    used_mb: 8188,
                    name: "llama-server".into()
                },
            ]
        );
    }

    #[test]
    fn skips_rows_with_unparseable_memory() {
        // `[N/A]` is what nvidia-smi prints for MIG / permission-denied rows.
        let apps = parse_compute_apps("1, [N/A], /usr/bin/x\n2, 512, /usr/bin/y\n");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].pid, 2);
    }

    #[test]
    fn self_vram_counts_only_this_process() {
        // A contended card: two other tenants hold 15 GB. Whole-card
        // `memory.used` would read 15.6 GB "resident"; the per-process
        // attribution must say 600 MB for us, 0 when we hold nothing.
        let apps = vec![
            ComputeApp {
                pid: 10,
                used_mb: 7000,
                name: "llama-server".into(),
            },
            ComputeApp {
                pid: 4242,
                used_mb: 600,
                name: "hs".into(),
            },
            ComputeApp {
                pid: 11,
                used_mb: 8000,
                name: "ollama".into(),
            },
        ];
        assert_eq!(vram_of_pid(&apps, 4242), 600);
        assert_eq!(vram_of_pid(&apps, 9999), 0, "visible card, not resident");
    }

    #[cfg(unix)]
    mod bounded_cmd {
        use super::*;

        const COOLDOWN: Duration = Duration::from_secs(60);

        #[test]
        fn returns_stdout_of_a_successful_command() {
            let cmd = BoundedCmd::new();
            let out = cmd.run("echo", &["hi"], Duration::from_secs(5), COOLDOWN);
            assert_eq!(out.as_deref(), Some("hi\n"));
        }

        #[test]
        fn missing_binary_and_nonzero_exit_are_none() {
            let cmd = BoundedCmd::new();
            assert!(cmd
                .run(
                    "definitely-not-a-real-binary-hs",
                    &[],
                    Duration::from_secs(1),
                    COOLDOWN
                )
                .is_none());
            assert!(cmd
                .run("false", &[], Duration::from_secs(5), COOLDOWN)
                .is_none());
        }

        /// RA-60: a wedged `nvidia-smi` must not hold the caller; the child
        /// is abandoned after the timeout and later calls fail fast for the
        /// cooldown instead of stacking another stuck child each.
        #[test]
        fn a_hung_command_times_out_then_trips_the_cooldown() {
            let cmd = BoundedCmd::new();
            let started = Instant::now();
            let out = cmd.run("sleep", &["30"], Duration::from_millis(150), COOLDOWN);
            assert!(out.is_none());
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "hung child blocked the caller for {:?}",
                started.elapsed()
            );

            // `echo` would succeed if spawned; the cooldown must short-circuit.
            let during = cmd.run("echo", &["hi"], Duration::from_secs(5), COOLDOWN);
            assert!(during.is_none(), "cooldown did not short-circuit");

            // A fresh runner (no timeout recorded) is unaffected.
            let fresh = BoundedCmd::new();
            assert_eq!(
                fresh
                    .run("echo", &["hi"], Duration::from_secs(5), COOLDOWN)
                    .as_deref(),
                Some("hi\n")
            );
        }
    }

    #[cfg(feature = "gpu-async")]
    mod async_cache {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        /// The refresh runs on a blocking thread: a single-worker runtime
        /// keeps ticking while a slow probe is in flight.
        #[tokio::test(flavor = "current_thread")]
        async fn slow_probe_does_not_block_the_runtime() {
            let cache: TtlCache<u64> = TtlCache::new();
            let ticks = Arc::new(AtomicUsize::new(0));
            let t = ticks.clone();
            let ticker = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    t.fetch_add(1, Ordering::Relaxed);
                }
            });
            let v = cache
                .get(Duration::from_secs(60), || {
                    std::thread::sleep(Duration::from_millis(300));
                    7
                })
                .await;
            ticker.abort();
            assert_eq!(v, 7);
            assert!(
                ticks.load(Ordering::Relaxed) >= 5,
                "runtime was starved during the probe: {} ticks",
                ticks.load(Ordering::Relaxed)
            );
        }

        #[tokio::test]
        async fn concurrent_callers_share_one_refresh_until_the_ttl_expires() {
            let cache: Arc<TtlCache<u64>> = Arc::new(TtlCache::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let ttl = Duration::from_millis(200);

            let mut tasks = Vec::new();
            for _ in 0..8 {
                let (cache, calls) = (cache.clone(), calls.clone());
                tasks.push(tokio::spawn(async move {
                    cache
                        .get(ttl, move || {
                            calls.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(50));
                            42
                        })
                        .await
                }));
            }
            for t in tasks {
                assert_eq!(t.await.unwrap(), 42);
            }
            assert_eq!(
                calls.load(Ordering::Relaxed),
                1,
                "refresh not single-flight"
            );

            tokio::time::sleep(ttl + Duration::from_millis(50)).await;
            let c = calls.clone();
            cache
                .get(ttl, move || {
                    c.fetch_add(1, Ordering::Relaxed);
                    43
                })
                .await;
            assert_eq!(
                calls.load(Ordering::Relaxed),
                2,
                "stale entry not refreshed"
            );
        }
    }
}
