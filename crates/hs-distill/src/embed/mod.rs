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

/// Is this error an out-of-memory failure from the accelerator?
///
/// ONNX Runtime reports a failed arena allocation as a node failure whose
/// message carries `Failed to allocate memory for requested buffer of
/// size N` (or `out of memory`). The shape is stable across the
/// versions in the field, and the alternative — treating every error
/// as an OOM — would halve the batch on a malformed input and hide the
/// real cause, so the message is matched, not guessed.
pub fn is_out_of_memory(err: &DistillError) -> bool {
    let DistillError::Embedding(message) = err else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    lower.contains("failed to allocate memory")
        || lower.contains("out of memory")
        || lower.contains("cuda error: out of memory")
        || lower.contains("allocating output")
}

/// Run `embed` over `texts` in groups of `batch_size` rows and concatenate
/// the results in order. Fails — rather than panics or silently drops
/// rows — on a zero batch size or an `embed` that returns the wrong number
/// of vectors.
///
/// **A batch that the accelerator cannot allocate is retried smaller.**
/// The scratch buffer a forward pass needs grows with the square of the
/// sequence length and linearly with the rows — at the server's 20-row
/// batch and 1280-token window it is 2.10 GB, which is the exact size
/// the arena names when it fails. A card that is shared with another
/// tenant cannot always give that block, and the answer is to use less
/// of the card, not to fail the document: measured on `big` 2026-10-21,
/// a 500 from a failed allocation was classified transient and
/// redelivered every 30 seconds forever, so the document never indexed.
///
/// The retry is safe because the caller upserts by `(doc_id,
/// chunk_index)`: a row embedded twice lands on the same key with the
/// same vector.
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
        let vectors = embed_with_smaller_batch(&rows, &mut embed)?;
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

/// Embed one batch, halving it when the accelerator cannot allocate the
/// scratch buffer for it.
///
/// Returns the error that stopped the descent, so the caller sees the
/// real cause rather than a generic one. The floor is one row: a single
/// row that cannot be allocated is not a batch-size problem, and
/// saying so is more useful than trying again.
///
/// The descent is per chunk, not per batch: when a 6-row batch has to
/// drop to 3, the remaining 3 rows are embedded at 3 — and if *that*
/// does not fit either, they descend again. A batch that only ever
/// tried its own width once would fail on the remainder.
fn embed_with_smaller_batch(
    rows: &[&str],
    embed: &mut impl FnMut(Vec<&str>) -> Result<Vec<Vec<f32>>, DistillError>,
) -> Result<Vec<Vec<f32>>, DistillError> {
    let mut out = Vec::with_capacity(rows.len());
    let mut start = 0;
    while start < rows.len() {
        let rest = &rows[start..];
        // Each chunk starts at the remainder's full size and descends
        // from there. The width is per chunk: a chunk that had to drop
        // to 1 must not shrink the *next* chunk's first attempt too.
        let mut width = rest.len();
        loop {
            let attempt: Vec<&str> = rest[..width].to_vec();
            match embed(attempt) {
                Ok(vectors) => {
                    if vectors.len() != width {
                        return Err(DistillError::Embedding(format!(
                            "embedder returned {} vectors for {} texts",
                            vectors.len(),
                            width
                        )));
                    }
                    out.extend(vectors);
                    start += width;
                    break;
                }
                Err(err) => {
                    if width == 1 || !is_out_of_memory(&err) {
                        return Err(err);
                    }
                    let next = (width / 2).max(1);
                    tracing::warn!(
                        from = width,
                        to = next,
                        "the accelerator could not allocate the batch's scratch buffer; \
                         retrying with a smaller batch"
                    );
                    width = next;
                }
            }
        }
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

    // ── the out-of-memory retry ────────────────────────────────────

    /// The error ONNX Runtime produces when the arena cannot allocate
    /// the batch's scratch buffer. The exact text is the one on `big`
    /// 2026-10-21.
    fn oom() -> DistillError {
        DistillError::Embedding(
            "Embedding failed: Non-zero status code returned while running Add node. \
             Status Message: bfc_arena.cc:359 void *onnxruntime::BFCArena::AllocateRawInternal(\
             size_t, bool, Stream *) Failed to allocate memory for requested buffer of size 2097152000"
                .into(),
        )
    }

    #[test]
    fn an_allocation_failure_is_recognised_as_one() {
        assert!(is_out_of_memory(&oom()));
        assert!(is_out_of_memory(&DistillError::Embedding(
            "CUDA error: out of memory".into()
        )));
        // A failure that is not an allocation failure must not be
        // mistaken for one: halving the batch on a malformed input would
        // hide the real cause.
        assert!(!is_out_of_memory(&DistillError::Embedding(
            "Embedding failed: input must be a string".into()
        )));
        assert!(!is_out_of_memory(&DistillError::Config("bad".into())));
    }

    #[test]
    fn a_batch_the_accelerator_cannot_allocate_is_retried_smaller() {
        // Measured on `big` 2026-10-21: the server's 20-row batch
        // needed a 2.10 GB scratch buffer the shared card could not
        // give, and the document was redelivered every 30 seconds
        // forever. The batch must shrink instead.
        let mut attempts = Vec::new();
        let out = embed_in_batches(&texts(8), 8, |rows| {
            attempts.push(rows.len());
            if rows.len() > 4 {
                return Err(oom());
            }
            Ok(rows.iter().map(|r| vec![r.len() as f32]).collect())
        })
        .unwrap();

        assert_eq!(out.len(), 8, "every text must still be embedded");
        assert_eq!(
            attempts,
            [8, 4, 4],
            "the batch must halve once and then complete"
        );
    }

    #[test]
    fn a_batch_that_fits_on_the_second_try_still_covers_every_text() {
        // The descent is per chunk, so the remainder of a batch that had
        // to shrink descends again if it has to. The widths are: the
        // whole batch, then 3, then 1 (the remainder of 3 does not fit
        // either), then the remaining 5, 2, 3, 1, 2.
        let mut attempts = Vec::new();
        // The vector carries the *row's* identity, so the order can be
        // checked: a shrunk batch must not reorder or drop a row.
        let out = embed_in_batches(&texts(6), 6, |rows| {
            attempts.push(rows.len());
            if rows.len() > 2 {
                return Err(oom());
            }
            Ok(rows
                .iter()
                .map(|r| vec![r.trim_start_matches('t').parse::<f32>().unwrap()])
                .collect())
        })
        .unwrap();
        assert_eq!(out.len(), 6);
        assert_eq!(attempts, [6, 3, 1, 5, 2, 3, 1, 2]);
        // The order is preserved: the vectors come back in input order,
        // which is what the caller upserts by.
        let ids: Vec<f32> = out.iter().map(|v| v[0]).collect();
        assert_eq!(ids, vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn a_failure_that_is_not_an_allocation_failure_is_not_retried() {
        // A malformed input must fail at once: retrying it at a smaller
        // size would hide the real cause behind a batch-size story.
        let mut attempts = 0;
        let err = embed_in_batches(&texts(4), 4, |_| {
            attempts += 1;
            Err(DistillError::Embedding(
                "Embedding failed: bad input".into(),
            ))
        })
        .unwrap_err();
        assert_eq!(attempts, 1, "the batch must not be retried");
        assert!(err.to_string().contains("bad input"), "{err}");
    }

    #[test]
    fn a_single_row_that_cannot_be_allocated_reports_the_real_cause() {
        // Below the floor there is nothing to shrink to. The error must
        // be the real one, not a generic "it failed".
        let err = embed_in_batches(&texts(1), 1, |_| Err(oom())).unwrap_err();
        assert!(is_out_of_memory(&err), "{err}");
    }
}
