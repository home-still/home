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

        Ok(Self {
            model: Mutex::new(model),
            device,
            dimension: config.dimension,
            batch_size,
        })
    }
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
