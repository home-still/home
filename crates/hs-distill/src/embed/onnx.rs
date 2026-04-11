use std::sync::Mutex;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::{ComputeDevice, Embedder};
use crate::config::EmbeddingConfig;
use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// ONNX-based embedder using fastembed-rs.
pub struct OnnxEmbedder {
    model: Mutex<TextEmbedding>,
    device: ComputeDevice,
    dimension: usize,
    batch_size: usize,
}

impl OnnxEmbedder {
    pub fn new(config: &EmbeddingConfig, device: ComputeDevice) -> Result<Self, DistillError> {
        let batch_size = config.batch_size.unwrap_or(match &device {
            ComputeDevice::Cpu => 8,
            ComputeDevice::Cuda => 32,
        });

        let mut opts = InitOptions::new(EmbeddingModel::BGEM3).with_show_download_progress(true);

        if matches!(device, ComputeDevice::Cuda) {
            use ort::execution_providers::CUDAExecutionProvider;
            opts = opts.with_execution_providers(vec![CUDAExecutionProvider::default().build()]);
        }

        let model = TextEmbedding::try_new(opts)
            .map_err(|e| DistillError::Embedding(format!("Failed to load model: {e}")))?;

        // Verify the requested device actually works by running a probe embedding.
        // ONNX Runtime silently falls back to CPU if CUDA fails to initialize,
        // so we measure wall-clock time of a probe to detect this.
        if matches!(device, ComputeDevice::Cuda) {
            tracing::info!("Verifying CUDA is actually being used (probe embedding)...");
            let probe_texts = vec!["CUDA verification probe"];
            let start = std::time::Instant::now();
            model
                .embed(probe_texts.clone(), None)
                .map_err(|e| DistillError::Embedding(format!("CUDA probe failed: {e}")))?;
            let probe_ms = start.elapsed().as_millis();

            // A GPU probe on a warm model completes in <100ms typically.
            // CPU on a 24-core machine takes ~200-500ms for a single text.
            // We also check nvidia-smi for actual GPU memory usage.
            let gpu_mem_used = check_gpu_memory_mb();
            tracing::info!(
                probe_ms = probe_ms,
                gpu_mem_mb = gpu_mem_used,
                "CUDA probe complete"
            );

            // If GPU memory didn't increase meaningfully, the model isn't on GPU
            if gpu_mem_used < 200 {
                return Err(DistillError::Embedding(format!(
                    "CUDA requested but model is not on GPU (only {gpu_mem_used} MB VRAM used). \
                     Check CUDA drivers, LD_LIBRARY_PATH, and libonnxruntime_providers_cuda.so. \
                     Set compute_device: cpu in config to run on CPU intentionally."
                )));
            }
            tracing::info!("CUDA verified: model loaded on GPU ({gpu_mem_used} MB VRAM)");
        }

        Ok(Self {
            model: Mutex::new(model),
            device,
            dimension: config.dimension,
            batch_size,
        })
    }
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

        // fastembed embed is blocking, run in spawn_blocking
        let texts = texts.to_vec();
        let batch_size = self.batch_size;

        // Process in sub-batches
        let mut all_outputs = Vec::with_capacity(texts.len());
        for batch_start in (0..texts.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(texts.len());
            let batch: Vec<&str> = texts[batch_start..batch_end]
                .iter()
                .map(|s| s.as_str())
                .collect();

            let embeddings = {
                let mut model = self
                    .model
                    .lock()
                    .map_err(|e| DistillError::Embedding(format!("Model lock poisoned: {e}")))?;
                model
                    .embed(batch, None)
                    .map_err(|e| DistillError::Embedding(format!("Embedding failed: {e}")))?
            };

            for dense in embeddings {
                all_outputs.push(EmbeddingOutput {
                    dense,
                    sparse: None, // TODO: BGE-M3 sparse output
                });
            }
        }

        Ok(all_outputs)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn supports_sparse(&self) -> bool {
        false // TODO: Enable when BGE-M3 sparse is supported
    }

    fn device(&self) -> &ComputeDevice {
        &self.device
    }
}
