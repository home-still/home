use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use hs_common::hardware_profile::HardwareProfile;

use super::{ComputeDevice, Embedder};
use crate::adaptive_batch::{AdaptiveBatchController, AdaptiveConfig};
use crate::config::EmbeddingConfig;
use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// ONNX-based embedder using fastembed-rs.
///
/// Owns a pool of `TextEmbedding` instances picked round-robin so
/// concurrent `embed_batch` callers don't contend on one Mutex. CUDA
/// hosts run one instance (single GPU context); CPU hosts run N where
/// N = `min(HardwareProfile::distill_concurrency, 4)`.
pub struct OnnxEmbedder {
    models: Vec<Arc<Mutex<TextEmbedding>>>,
    next: AtomicUsize,
    device: ComputeDevice,
    dimension: usize,
    batch_ctrl: Arc<AdaptiveBatchController>,
}

impl OnnxEmbedder {
    pub fn new(config: &EmbeddingConfig, device: ComputeDevice) -> Result<Self, DistillError> {
        let initial_batch_size = config.batch_size.unwrap_or(match &device {
            ComputeDevice::Cpu => 8,
            ComputeDevice::Cuda => 32,
        });

        let pool_size = config.pool_size.unwrap_or_else(|| match &device {
            ComputeDevice::Cuda => 1,
            ComputeDevice::Cpu => {
                let hw = HardwareProfile::detect();
                hw.class.distill_concurrency(hw.cpu_count).min(4)
            }
        });
        let pool_size = pool_size.max(1);

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
            "initializing bge-m3 embedder pool"
        );

        // Build the first model and, on CUDA, verify GPU residency before
        // allocating the rest. Probe failure aborts — one path, no silent
        // CPU substitute.
        let mut first = build_text_embedding(&device)?;
        if matches!(device, ComputeDevice::Cuda) {
            verify_cuda_probe(&mut first)?;
        }

        let mut models: Vec<Arc<Mutex<TextEmbedding>>> = Vec::with_capacity(pool_size);
        models.push(Arc::new(Mutex::new(first)));
        for _ in 1..pool_size {
            let model = build_text_embedding(&device)?;
            models.push(Arc::new(Mutex::new(model)));
        }

        Ok(Self {
            models,
            next: AtomicUsize::new(0),
            device,
            dimension: config.dimension,
            batch_ctrl: Arc::new(AdaptiveBatchController::new(adaptive_cfg)),
        })
    }
}

fn build_text_embedding(device: &ComputeDevice) -> Result<TextEmbedding, DistillError> {
    let mut opts = InitOptions::new(EmbeddingModel::BGEM3).with_show_download_progress(true);
    if matches!(device, ComputeDevice::Cuda) {
        use ort::execution_providers::CUDAExecutionProvider;
        opts = opts.with_execution_providers(vec![CUDAExecutionProvider::default().build()]);
    }
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
             Check CUDA drivers, LD_LIBRARY_PATH, and libonnxruntime_providers_cuda.so. \
             Set compute_device: cpu in config to run on CPU intentionally."
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

#[async_trait]
impl Embedder for OnnxEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

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
            let mut model = model
                .lock()
                .map_err(|e| DistillError::Embedding(format!("Model lock poisoned: {e}")))?;
            let mut out = Vec::with_capacity(texts.len());
            for batch_start in (0..texts.len()).step_by(batch_size) {
                let batch_end = (batch_start + batch_size).min(texts.len());
                let batch: Vec<&str> = texts[batch_start..batch_end]
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                let embeddings = model
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
