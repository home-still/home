pub mod pool;

#[cfg(feature = "server")]
pub mod onnx;

use async_trait::async_trait;

use crate::error::DistillError;
use crate::types::EmbeddingOutput;

// Re-export so existing `use crate::embed::ComputeDevice` paths continue
// to compile. Canonical home of the enum is `crate::config`, where it
// is referenced by `EmbeddingConfig::compute_device`.
pub use crate::config::ComputeDevice;

/// The one embedding model this server runs (fastembed's `BGEM3`). It is a
/// constant, not configuration: nothing could change it without changing
/// the code that loads it.
pub const MODEL_NAME: &str = "bge-m3";

/// Whether an embedder can still serve requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedderHealth {
    Healthy,
    /// Permanently unusable until the process restarts.
    Failed(String),
}

/// Trait for embedding backends.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed `texts`, one vector per text, in order. Takes ownership so the
    /// backend can move the strings into its worker thread without copying.
    async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<EmbeddingOutput>, DistillError>;
    /// Width of the vectors this embedder returns, as measured from the
    /// model's own output.
    fn dimension(&self) -> usize;
    fn device(&self) -> &ComputeDevice;
    fn health(&self) -> EmbedderHealth;
    /// How many `embed_batch` calls can run at once (model pool size).
    fn slots(&self) -> usize;
}

/// Compare the configured embedding width with the one the model actually
/// produced. A collection is created at the width the embedder reports, so
/// a config that disagrees with the model is a mistake to surface at
/// startup, not something to resolve silently in either direction.
pub fn check_dimension(configured: usize, measured: usize) -> Result<(), DistillError> {
    if configured == measured {
        return Ok(());
    }
    Err(DistillError::Config(format!(
        "embedding.dimension is {configured} but {MODEL_NAME} returned {measured}-wide vectors; \
         fix embedding.dimension (the model fixes the width)"
    )))
}

/// Run `embed` over `texts` in groups of `batch_size` rows and concatenate
/// the results in order. Fails — rather than panics or silently drops
/// rows — on a zero batch size or an `embed` that returns the wrong number
/// of vectors.
pub fn embed_in_batches(
    texts: &[String],
    batch_size: usize,
    mut embed: impl FnMut(Vec<&str>) -> Result<Vec<Vec<f32>>, DistillError>,
) -> Result<Vec<Vec<f32>>, DistillError> {
    if batch_size == 0 {
        return Err(DistillError::Config(
            "embedding batch size must be at least 1".into(),
        ));
    }
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(batch_size) {
        let rows: Vec<&str> = batch.iter().map(String::as_str).collect();
        let vectors = embed(rows)?;
        if vectors.len() != batch.len() {
            return Err(DistillError::Embedding(format!(
                "embedder returned {} vectors for {} texts",
                vectors.len(),
                batch.len()
            )));
        }
        out.extend(vectors);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i}")).collect()
    }

    #[test]
    fn a_dimension_that_disagrees_with_the_model_is_a_config_error() {
        check_dimension(1024, 1024).unwrap();
        let err = check_dimension(768, 1024).unwrap_err();
        assert!(matches!(err, DistillError::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("768") && msg.contains("1024"), "{msg}");
    }

    #[test]
    fn batches_cover_every_text_in_order() {
        let mut seen_sizes = Vec::new();
        let out = embed_in_batches(&texts(7), 3, |rows| {
            seen_sizes.push(rows.len());
            Ok(rows.iter().map(|r| vec![r.len() as f32]).collect())
        })
        .unwrap();
        assert_eq!(seen_sizes, [3, 3, 1]);
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn zero_batch_size_is_an_error_not_a_panic() {
        // `(0..n).step_by(0)` panics; this is the shape the embed thread had.
        let err = embed_in_batches(&texts(2), 0, |_| Ok(vec![])).unwrap_err();
        assert!(matches!(err, DistillError::Config(_)), "{err}");
    }

    #[test]
    fn short_batch_output_is_an_error_not_a_dropped_chunk() {
        let err = embed_in_batches(&texts(4), 4, |_| Ok(vec![vec![0.0]; 3])).unwrap_err();
        assert!(matches!(err, DistillError::Embedding(_)), "{err}");
    }

    #[test]
    fn empty_input_embeds_nothing() {
        let out = embed_in_batches(&[], 8, |_| unreachable!("no batch to run")).unwrap();
        assert!(out.is_empty());
    }
}
