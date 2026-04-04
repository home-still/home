pub mod onnx;

use async_trait::async_trait;

use crate::error::DistillError;
use crate::types::EmbeddingOutput;

/// Compute device for embedding inference.
#[derive(Debug, Clone)]
pub enum ComputeDevice {
    Cpu,
}

impl std::fmt::Display for ComputeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComputeDevice::Cpu => write!(f, "Cpu"),
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
pub fn detect_device() -> ComputeDevice {
    // TODO: Add CUDA/Metal detection when those features are implemented
    ComputeDevice::Cpu
}

/// Wraps a primary (GPU) and fallback (CPU) embedder.
/// On batch failure from primary, retries on fallback.
pub struct FallbackEmbedder {
    primary: Box<dyn Embedder>,
    fallback: Option<Box<dyn Embedder>>,
    consecutive_failures: std::sync::atomic::AtomicU32,
}

impl FallbackEmbedder {
    pub fn new(primary: Box<dyn Embedder>, fallback: Option<Box<dyn Embedder>>) -> Self {
        Self {
            primary,
            fallback,
            consecutive_failures: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Build the right embedder configuration based on detected device.
    pub fn build(config: &crate::config::EmbeddingConfig) -> Result<Self, DistillError> {
        let device = detect_device();
        tracing::info!("Detected compute device: {}", device);

        let primary = onnx::OnnxEmbedder::new(config, device.clone())?;
        Ok(Self::new(Box::new(primary), None))
    }
}

#[async_trait]
impl Embedder for FallbackEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError> {
        match self.primary.embed_batch(texts).await {
            Ok(result) => {
                self.consecutive_failures
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                Ok(result)
            }
            Err(e) => {
                let failures = self
                    .consecutive_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;

                if let Some(fallback) = &self.fallback {
                    tracing::warn!(
                        error = %e,
                        failures = failures,
                        "Primary embedder failed, falling back to CPU"
                    );
                    fallback.embed_batch(texts).await
                } else {
                    Err(e)
                }
            }
        }
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
