pub mod onnx;

use async_trait::async_trait;

use crate::error::DistillError;
use crate::types::EmbeddingOutput;

// Re-export so existing `use crate::embed::ComputeDevice` paths continue
// to compile. Canonical home of the enum is `crate::config`, where it
// is referenced by `EmbeddingConfig::compute_device`.
pub use crate::config::ComputeDevice;

/// Trait for embedding backends.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<EmbeddingOutput>, DistillError>;
    fn dimension(&self) -> usize;
    fn supports_sparse(&self) -> bool;
    fn device(&self) -> &ComputeDevice;
}

/// Wraps the selected embedder. The name `FallbackEmbedder` is historical;
/// there is no fallback path — if CUDA fails, startup fails.
pub struct FallbackEmbedder {
    primary: Box<dyn Embedder>,
}

impl FallbackEmbedder {
    pub fn new(primary: Box<dyn Embedder>, _fallback: Option<Box<dyn Embedder>>) -> Self {
        Self { primary }
    }

    /// Build the embedder strictly according to the configured device.
    /// `OnnxEmbedder::new` verifies the CUDA probe succeeds; failure is
    /// propagated — no silent CPU fallback (ONE PATH).
    pub fn build(config: &crate::config::EmbeddingConfig) -> Result<Self, DistillError> {
        tracing::info!("Using configured compute device: {}", config.compute_device);
        let primary = onnx::OnnxEmbedder::new(config, config.compute_device.clone())?;
        Ok(Self::new(Box::new(primary), None))
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
