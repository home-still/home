use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::pool::{is_idle, SlotError, SlotPool};
use super::{embed_in_batches, ComputeDevice, Embedder, EmbedderHealth};
use crate::adaptive_batch::{AdaptiveBatchController, AdaptiveConfig};
use crate::config::{max_batch_rows, EmbeddingConfig};
use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// ONNX-based embedder using fastembed-rs. Always runs on CUDA — the
/// distill binary ships with no CPU code path (rc.306 P0-7).
///
/// Each pool slot is `Mutex<Option<TextEmbedding>>` (see
/// [`SlotPool`]) rather than `Mutex<TextEmbedding>` so an idle-sweeper task
/// can drop the loaded model and release the CUDA allocations under it. The
/// next `embed_batch` call lazily rebuilds. This is opt-in via
/// `EmbeddingConfig::idle_release_secs` — `None` keeps the original
/// always-resident behavior.
pub struct OnnxEmbedder {
    pool: Arc<SlotPool<TextEmbedding>>,
    device: ComputeDevice,
    /// Measured from the model's own output at startup.
    dimension: usize,
    max_length: usize,
    batch_ctrl: Arc<AdaptiveBatchController>,
    /// Unix millis of the most recent `embed_batch` entry. Stamped at
    /// the start of the call, not the end, so a long-running embed
    /// keeps the model warm for its own duration without the sweeper
    /// racing it to release.
    last_used_ms: Arc<AtomicI64>,
    /// Free-VRAM floor enforced before every (re)load. Copied from
    /// config so the lazy-reload path inside `spawn_blocking` can apply
    /// the same gate as startup.
    vram_floor_mb: u64,
}

impl OnnxEmbedder {
    pub fn new(config: &EmbeddingConfig, device: ComputeDevice) -> Result<Self, DistillError> {
        config.validate_self()?;
        // Fixed CUDA tuning — the previous match on ComputeDevice::Cpu is
        // gone (no CPU variant exists). Callers can still override via
        // config.batch_size / config.pool_size.
        let initial_batch_size = config.initial_batch();
        let pool_size = config.pool_size.unwrap_or(1);

        let adaptive_cfg = if config.adaptive_batch {
            AdaptiveConfig::default_for_device(
                &device,
                initial_batch_size,
                max_batch_rows(config.max_length),
            )
        } else {
            AdaptiveConfig::pinned(initial_batch_size)
        };

        tracing::info!(
            device = %device,
            pool_size,
            initial_batch_size,
            max_length = config.max_length,
            adaptive = config.adaptive_batch,
            candidates = ?adaptive_cfg.candidates,
            idle_release_secs = ?config.idle_release_secs,
            "initializing bge-m3 embedder pool"
        );

        // Refuse the load outright when the card is already spoken for,
        // then build the first model and verify GPU residency before
        // allocating the rest. Either failure aborts — one path, no
        // silent CPU substitute.
        require_vram(config.vram_floor_mb)?;
        let mut first = build_text_embedding(config.max_length)?;
        let dimension = verify_cuda_probe(&mut first)?;
        if dimension != config.dimension {
            return Err(DistillError::Config(format!(
                "embedding.dimension is {} but bge-m3 returned {dimension}-wide vectors; \
                 a collection created at the configured width would reject every point",
                config.dimension
            )));
        }

        let mut rest = Vec::with_capacity(pool_size - 1);
        for _ in 1..pool_size {
            rest.push(build_text_embedding(config.max_length)?);
        }
        let pool = Arc::new(SlotPool::new(first, rest));

        let last_used_ms = Arc::new(AtomicI64::new(now_unix_ms()));

        if let Some(idle_secs) = config.idle_release_secs {
            spawn_idle_sweeper(pool.clone(), last_used_ms.clone(), idle_secs);
        }

        Ok(Self {
            pool,
            device,
            dimension,
            max_length: config.max_length,
            batch_ctrl: Arc::new(AdaptiveBatchController::new(adaptive_cfg)),
            last_used_ms,
            vram_floor_mb: config.vram_floor_mb,
        })
    }
}

fn build_text_embedding(max_length: usize) -> Result<TextEmbedding, DistillError> {
    use ort::execution_providers::CUDAExecutionProvider;
    // error_on_failure: ort's default is to log and silently fall back to
    // the CPU provider when CUDA registration fails. Distill ships with
    // no CPU path — a failed registration must be a hard error, not a
    // 50x-slower session that only the (startup-only) VRAM probe could
    // have caught.
    //
    // with_max_length: fastembed defaults to 512 tokens and truncates in
    // the tokenizer, which silently dropped the back half of ~1000-token
    // chunks from their vectors.
    let opts = InitOptions::new(EmbeddingModel::BGEM3)
        .with_max_length(max_length)
        .with_show_download_progress(true)
        .with_execution_providers(vec![CUDAExecutionProvider::default()
            .build()
            .error_on_failure()]);
    TextEmbedding::try_new(opts)
        .map_err(|e| DistillError::Embedding(format!("Failed to load model: {e}")))
}

/// Verify CUDA residency via wall-clock + VRAM probe. Fails loud if ONNX
/// silently dropped to CPU. Returns the width of the vectors the model
/// actually produced.
fn verify_cuda_probe(model: &mut TextEmbedding) -> Result<usize, DistillError> {
    tracing::info!("Verifying CUDA is actually being used (probe embedding)...");
    let probe_texts = vec!["CUDA verification probe"];
    let start = std::time::Instant::now();
    let probe = model
        .embed(probe_texts, None)
        .map_err(|e| DistillError::Embedding(format!("CUDA probe failed: {e}")))?;
    let probe_ms = start.elapsed().as_millis();
    let dimension = probe
        .first()
        .map(Vec::len)
        .filter(|&d| d > 0)
        .ok_or_else(|| DistillError::Embedding("CUDA probe produced no vector".into()))?;

    let self_mem = hs_common::gpu::self_vram_mb();
    tracing::info!(
        probe_ms = probe_ms,
        self_vram_mb = ?self_mem,
        dimension,
        "CUDA probe complete"
    );

    // Per-process attribution, not whole-card `memory.used`: on a
    // contended card the old check read 23 GB of OTHER tenants'
    // allocations and passed vacuously while this process sat on CPU.
    if self_mem.is_none_or(|mb| mb < 200) {
        return Err(DistillError::Embedding(format!(
            "CUDA requested but model is not on GPU (own process VRAM: {self_mem:?} MB). \
             Fix CUDA: check driver, LD_LIBRARY_PATH, libonnxruntime_providers_cuda.so, \
             and the pyke ort cache (~/.cache/ort.pyke.io/dfbin). Distill ships with no CPU path."
        )));
    }
    tracing::info!("CUDA verified: model loaded on GPU ({self_mem:?} MB VRAM, this process)");
    Ok(dimension)
}

/// Refuse to load the embedder when the card cannot host it. `None`
/// free-VRAM means no NVIDIA GPU is visible — no gate to apply — which
/// keeps this a no-op on non-CUDA hosts while still failing loudly on
/// `big` when a foreign tenant owns the card.
fn require_vram(floor_mb: u64) -> Result<(), DistillError> {
    let Some(free) = hs_common::gpu::free_vram_mb() else {
        return Ok(());
    };
    if free < floor_mb {
        return Err(DistillError::Embedding(format!(
            "gpu busy: {free} MB free < {floor_mb} MB required to load bge-m3; holders: {}",
            hs_common::gpu::compute_apps_summary()
        )));
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A panic inside ONNX Runtime leaves the session in an unknown state and
/// poisons its slot; no later request could succeed while `/health` kept
/// answering 200. Exit loudly so the supervisor restarts a clean process.
fn exit_on_unusable_embedder(why: &str) -> ! {
    tracing::error!(
        why,
        "embedder is unusable; exiting so the supervisor restarts the server"
    );
    eprintln!("hs-distill-server: embedder is unusable ({why}); exiting");
    std::process::exit(i32::from(hs_common::exit_codes::GENERAL_ERROR))
}

/// Background task that drops each pool slot's `TextEmbedding` after the
/// configured idle window. Wakes every 30 s; cheap relative to the cost
/// of holding ~5 GB of VRAM. Logs each release with the observed idle
/// time so an operator can see whether the timeout is well-tuned.
///
/// The release itself runs on the blocking pool: dropping a CUDA session
/// can take a while and must not stall a runtime worker.
fn spawn_idle_sweeper(
    pool: Arc<SlotPool<TextEmbedding>>,
    last_used_ms: Arc<AtomicI64>,
    idle_secs: u64,
) {
    let idle_ms = idle_secs.saturating_mul(1000) as i64;
    let sweep_interval = Duration::from_secs(30);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(sweep_interval).await;
            let now = now_unix_ms();
            let last = last_used_ms.load(Ordering::Relaxed);
            if !is_idle(now, last, idle_ms) {
                continue;
            }
            let sweep_pool = Arc::clone(&pool);
            let swept = match tokio::task::spawn_blocking(move || sweep_pool.release_idle()).await {
                Ok(swept) => swept,
                Err(e) => {
                    tracing::error!(error = %e, "idle sweeper task failed");
                    continue;
                }
            };
            if swept.poisoned > 0 {
                tracing::warn!(
                    poisoned_slots = swept.poisoned,
                    "idle sweeper: skipped poisoned slot(s)"
                );
            }
            if swept.released > 0 {
                tracing::info!(
                    idle_secs = now.saturating_sub(last) / 1000,
                    released_slots = swept.released,
                    "released bge-m3 embedder pool after idle window"
                );
                // Bump last_used so we don't re-log every 30 s while no
                // requests are coming in. The next embed_batch will set
                // its own timestamp before reloading.
                last_used_ms.store(now, Ordering::Relaxed);
            }
        }
    });
}

#[async_trait]
impl Embedder for OnnxEmbedder {
    async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<EmbeddingOutput>, DistillError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Stamp last_used at the START of the call so a long-running
        // embed keeps the model warm for its own duration. Stamping at
        // the end would race the sweeper.
        self.last_used_ms.store(now_unix_ms(), Ordering::Relaxed);

        let texts_len = texts.len();
        let batch_size = self.batch_ctrl.current();
        let pool = Arc::clone(&self.pool);
        let (vram_floor_mb, max_length, dimension) =
            (self.vram_floor_mb, self.max_length, self.dimension);

        // fastembed's `embed` is synchronous and CPU/GPU-heavy. spawn_blocking
        // keeps it off the tokio worker threads. `texts` moves in; nothing is
        // copied.
        let started = Instant::now();
        let joined = tokio::task::spawn_blocking(move || {
            pool.run(
                // Lazy-load if the idle sweeper dropped the model. First
                // request after release pays the ~10 s load cost; subsequent
                // requests are fast.
                || {
                    // Same free-VRAM gate as startup. Returning the error
                    // here makes `hs distill watch-events` NAK so JetStream
                    // redelivers once the card frees up — a CUDA OOM at this
                    // point poisons the session for every later request.
                    require_vram(vram_floor_mb)?;
                    tracing::info!("lazy-loading bge-m3 after idle release");
                    let mut m = build_text_embedding(max_length)?;
                    // Same CUDA-residency gate as startup: a driver hiccup or
                    // evicted pyke cache between idle-release and rebuild must
                    // fail loudly here, not degrade every subsequent embed to
                    // CPU until someone notices the throughput graph.
                    let reloaded = verify_cuda_probe(&mut m)?;
                    if reloaded != dimension {
                        return Err(DistillError::Embedding(format!(
                            "reloaded model returned {reloaded}-wide vectors, expected {dimension}"
                        )));
                    }
                    Ok(m)
                },
                |model| {
                    embed_in_batches(&texts, batch_size, |rows| {
                        model
                            .embed(rows, None)
                            .map_err(|e| DistillError::Embedding(format!("Embedding failed: {e}")))
                    })
                },
            )
        })
        .await;

        let vectors = match joined {
            Ok(Ok(vectors)) => vectors,
            Ok(Err(SlotError::Failed(e))) => return Err(e),
            Ok(Err(SlotError::Poisoned { slot })) => exit_on_unusable_embedder(&format!(
                "model slot {slot} poisoned by an earlier panic"
            )),
            Err(e) if e.is_panic() => {
                exit_on_unusable_embedder("panic inside the embedding thread")
            }
            Err(e) => {
                return Err(DistillError::Embedding(format!(
                    "embedding task did not complete: {e}"
                )))
            }
        };

        // Feed the controller: texts_len processed in elapsed wall-clock.
        self.batch_ctrl
            .observe(texts_len, started.elapsed().as_secs_f64());

        Ok(vectors
            .into_iter()
            .map(|dense| EmbeddingOutput { dense })
            .collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn device(&self) -> &ComputeDevice {
        &self.device
    }

    fn health(&self) -> EmbedderHealth {
        match self.pool.poisoned_slot() {
            None => EmbedderHealth::Healthy,
            Some(slot) => EmbedderHealth::Failed(format!(
                "model slot {slot} was poisoned by a panic inside ONNX Runtime"
            )),
        }
    }

    fn slots(&self) -> usize {
        self.pool.len()
    }
}
