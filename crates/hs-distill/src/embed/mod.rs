pub mod onnx;

use async_trait::async_trait;

use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// Compute device for embedding inference.
#[derive(Debug, Clone)]
pub enum ComputeDevice {
    Cpu,
    Cuda,
}

impl std::fmt::Display for ComputeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComputeDevice::Cpu => write!(f, "Cpu"),
            ComputeDevice::Cuda => write!(f, "Cuda"),
        }
    }
}

/// Trait for embedding backends.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError>;
    fn dimension(&self) -> usize;
    fn supports_sparse(&self) -> bool;
    fn device(&self) -> &ComputeDevice;
}

/// Detect available compute device at startup.
/// Checks that nvidia-smi succeeds AND reports at least one GPU.
pub fn detect_device() -> ComputeDevice {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok();

    match output {
        Some(o) if o.status.success() => {
            let gpu_name = String::from_utf8_lossy(&o.stdout);
            let gpu_name = gpu_name.trim();
            if gpu_name.is_empty() {
                tracing::warn!("nvidia-smi succeeded but reported no GPUs");
                ComputeDevice::Cpu
            } else {
                tracing::info!("Detected GPU: {gpu_name}");
                ComputeDevice::Cuda
            }
        }
        _ => {
            tracing::info!("No NVIDIA GPU detected (nvidia-smi not found or failed)");
            ComputeDevice::Cpu
        }
    }
}

/// Wraps the selected embedder (GPU or CPU). No silent fallback —
/// if the configured device doesn't work, startup fails.
pub struct FallbackEmbedder {
    primary: Box<dyn Embedder>,
}

impl FallbackEmbedder {
    pub fn new(primary: Box<dyn Embedder>, _fallback: Option<Box<dyn Embedder>>) -> Self {
        Self { primary }
    }

    /// Build the right embedder configuration based on detected device.
    /// If GPU is detected, CUDA must actually work — no silent CPU fallback.
    pub fn build(config: &crate::config::EmbeddingConfig) -> Result<Self, DistillError> {
        let device = detect_device();
        tracing::info!("Detected compute device: {}", device);

        match device {
            ComputeDevice::Cuda => {
                // OnnxEmbedder::new will verify CUDA actually works (probe embedding).
                // If CUDA fails, it returns an error — no silent fallback.
                let primary = onnx::OnnxEmbedder::new(config, ComputeDevice::Cuda)?;
                tracing::info!("GPU embedder initialized (no CPU fallback)");
                Ok(Self::new(Box::new(primary), None))
            }
            ComputeDevice::Cpu => {
                let primary = onnx::OnnxEmbedder::new(config, ComputeDevice::Cpu)?;
                tracing::info!("CPU embedder initialized");
                Ok(Self::new(Box::new(primary), None))
            }
        }
    }
}

#[async_trait]
impl Embedder for FallbackEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError> {
        self.primary.embed_batch(texts).await
    }

    fn dimension(&self) -> usize {
        self.primary.dimension()
    }

    fn supports_sparse(&self) -> bool {
        self.primary.supports_sparse()
    }

    fn device(&self) -> &ComputeDevice {
        self.primary.device()
    }
}
