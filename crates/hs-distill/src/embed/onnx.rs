use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::{ComputeDevice, Embedder};
use crate::adaptive_batch::{AdaptiveBatchController, AdaptiveConfig};
use crate::config::EmbeddingConfig;
use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// ONNX-based embedder using fastembed-rs. Always runs on CUDA — the
/// distill binary ships with no CPU code path (rc.306 P0-7).
///
/// Each pool slot is `Mutex<Option<TextEmbedding>>` rather than
/// `Mutex<TextEmbedding>` so an idle-sweeper task can `take()` the
/// loaded model and let it Drop, releasing the CUDA allocations under
/// it. The next `embed_batch` call lazily rebuilds. This is opt-in via
/// `EmbeddingConfig::idle_release_secs` — `None` keeps the original
/// always-resident behavior.
pub struct OnnxEmbedder {
    models: Vec<Arc<Mutex<Option<TextEmbedding>>>>,
    next: AtomicUsize,
    device: ComputeDevice,
    dimension: usize,
    batch_ctrl: Arc<AdaptiveBatchController>,
    /// Unix millis of the most recent `embed_batch` entry. Stamped at
    /// the start of the call, not the end, so a long-running embed
    /// keeps the model warm for its own duration without the sweeper
    /// racing it to release.
    last_used_ms: Arc<AtomicI64>,
}

impl OnnxEmbedder {
    pub fn new(config: &EmbeddingConfig, device: ComputeDevice) -> Result<Self, DistillError> {
        // Fixed CUDA tuning — the previous match on ComputeDevice::Cpu is
        // gone (no CPU variant exists). Callers can still override via
        // config.batch_size / config.pool_size.
        let initial_batch_size = config.batch_size.unwrap_or(32);
        let pool_size = config.pool_size.unwrap_or(1).max(1);

        let adaptive_cfg = if config.adaptive_batch {
            AdaptiveConfig::default_for_device(&device, initial_batch_size)
        } else {
            AdaptiveConfig::pinned(initial_batch_size)
        };

        tracing::info!(
            device = %device,
            pool_size,
            initial_batch_size,
            adaptive = config.adaptive_batch,
            candidates = ?adaptive_cfg.candidates,
            idle_release_secs = ?config.idle_release_secs,
            "initializing bge-m3 embedder pool"
        );

        // Build the first model and verify GPU residency before allocating
        // the rest. Probe failure aborts — one path, no silent CPU
        // substitute.
        let mut first = build_text_embedding()?;
        verify_cuda_probe(&mut first)?;

        let mut models: Vec<Arc<Mutex<Option<TextEmbedding>>>> = Vec::with_capacity(pool_size);
        models.push(Arc::new(Mutex::new(Some(first))));
        for _ in 1..pool_size {
            let model = build_text_embedding()?;
            models.push(Arc::new(Mutex::new(Some(model))));
        }

        let last_used_ms = Arc::new(AtomicI64::new(now_unix_ms()));

        if let Some(idle_secs) = config.idle_release_secs {
            spawn_idle_sweeper(models.clone(), last_used_ms.clone(), idle_secs);
        }

        Ok(Self {
            models,
            next: AtomicUsize::new(0),
            device,
            dimension: config.dimension,
            batch_ctrl: Arc::new(AdaptiveBatchController::new(adaptive_cfg)),
            last_used_ms,
        })
    }
}

fn build_text_embedding() -> Result<TextEmbedding, DistillError> {
    use ort::execution_providers::CUDAExecutionProvider;
    // error_on_failure: ort's default is to log and silently fall back to
    // the CPU provider when CUDA registration fails. Distill ships with
    // no CPU path — a failed registration must be a hard error, not a
    // 50x-slower session that only the (startup-only) VRAM probe could
    // have caught.
    let opts = InitOptions::new(EmbeddingModel::BGEM3)
        .with_show_download_progress(true)
        .with_execution_providers(vec![CUDAExecutionProvider::default()
            .build()
            .error_on_failure()]);
    TextEmbedding::try_new(opts)
        .map_err(|e| DistillError::Embedding(format!("Failed to load model: {e}")))
}

/// Verify CUDA residency via wall-clock + VRAM probe. Fails loud if ONNX
/// silently dropped to CPU.
fn verify_cuda_probe(model: &mut TextEmbedding) -> Result<(), DistillError> {
    tracing::info!("Verifying CUDA is actually being used (probe embedding)...");
    let probe_texts = vec!["CUDA verification probe"];
    let start = std::time::Instant::now();
    model
        .embed(probe_texts, None)
        .map_err(|e| DistillError::Embedding(format!("CUDA probe failed: {e}")))?;
    let probe_ms = start.elapsed().as_millis();

    let gpu_mem_used = check_gpu_memory_mb();
    tracing::info!(
        probe_ms = probe_ms,
        gpu_mem_mb = gpu_mem_used,
        "CUDA probe complete"
    );

    if gpu_mem_used < 200 {
        return Err(DistillError::Embedding(format!(
            "CUDA requested but model is not on GPU (only {gpu_mem_used} MB VRAM used). \
             Fix CUDA: check driver, LD_LIBRARY_PATH, libonnxruntime_providers_cuda.so, \
             and the pyke ort cache (~/.cache/ort.pyke.io/dfbin). Distill ships with no CPU path."
        )));
    }
    tracing::info!("CUDA verified: model loaded on GPU ({gpu_mem_used} MB VRAM)");
    Ok(())
}

/// Check GPU memory usage via nvidia-smi. Returns MB used, or 0 on failure.
fn check_gpu_memory_mb() -> u64 {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .ok();
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Background task that drops each pool slot's `TextEmbedding` after the
/// configured idle window. Wakes every 30 s; cheap relative to the cost
/// of holding ~5 GB of VRAM. Logs each release with the observed idle
/// time so an operator can see whether the timeout is well-tuned.
fn spawn_idle_sweeper(
    models: Vec<Arc<Mutex<Option<TextEmbedding>>>>,
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
            let idle = now.saturating_sub(last);
            if idle < idle_ms {
                continue;
            }
            let mut released = 0usize;
            for slot in &models {
                let mut guard = match slot.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(error = %e, "idle sweeper: lock poisoned; skipping slot");
                        continue;
                    }
                };
                if guard.take().is_some() {
                    released += 1;
                }
            }
            if released > 0 {
                tracing::info!(
                    idle_secs = idle / 1000,
                    released_slots = released,
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
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Stamp last_used at the START of the call so a long-running
        // embed keeps the model warm for its own duration. Stamping at
        // the end would race the sweeper.
        self.last_used_ms.store(now_unix_ms(), Ordering::Relaxed);

        let texts_len = texts.len();
        let texts: Vec<String> = texts.to_vec();
        let batch_size = self.batch_ctrl.current();

        // Round-robin pick a model so concurrent callers land on different
        // Mutexes on CPU hosts. CUDA hosts have pool_size=1 so this just
        // picks index 0.
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.models.len();
        let model = Arc::clone(&self.models[idx]);

        // fastembed's `embed` is synchronous and CPU/GPU-heavy. spawn_blocking
        // keeps it off the tokio worker threads.
        let started = Instant::now();
        let denses = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>, DistillError> {
            let mut guard = model
                .lock()
                .map_err(|e| DistillError::Embedding(format!("Model lock poisoned: {e}")))?;

            // Lazy-load if the idle sweeper dropped the model. First
            // request after release pays the ~10 s load cost; subsequent
            // requests are fast.
            if guard.is_none() {
                tracing::info!("lazy-loading bge-m3 after idle release");
                let mut m = build_text_embedding()?;
                // Same CUDA-residency gate as startup: a driver hiccup or
                // evicted pyke cache between idle-release and rebuild must
                // fail loudly here, not degrade every subsequent embed to
                // CPU until someone notices the throughput graph.
                verify_cuda_probe(&mut m)?;
                *guard = Some(m);
            }
            let model_ref = guard
                .as_mut()
                .expect("model loaded above or already present");

            let mut out = Vec::with_capacity(texts.len());
            for batch_start in (0..texts.len()).step_by(batch_size) {
                let batch_end = (batch_start + batch_size).min(texts.len());
                let batch: Vec<&str> = texts[batch_start..batch_end]
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                let embeddings = model_ref
                    .embed(batch, None)
                    .map_err(|e| DistillError::Embedding(format!("Embedding failed: {e}")))?;
                out.extend(embeddings);
            }
            Ok(out)
        })
        .await
        .map_err(|e| DistillError::Embedding(format!("spawn_blocking join failed: {e}")))??;

        // Feed the controller: texts_len processed in elapsed wall-clock.
        self.batch_ctrl
            .observe(texts_len, started.elapsed().as_secs_f64());

        Ok(denses
            .into_iter()
            .map(|dense| EmbeddingOutput {
                dense,
                sparse: None,
            })
            .collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn supports_sparse(&self) -> bool {
        false
    }

    fn device(&self) -> &ComputeDevice {
        &self.device
    }
}
