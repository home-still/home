use std::collections::HashMap;

/// Compute BLEU-4 score with brevity penalty.
/// Returns a value in [0.0, 1.0].
pub fn bleu4(reference: &str, hypothesis: &str) -> f64 {
    let ref_tokens = tokenize(reference);
    let hyp_tokens = tokenize(hypothesis);

    if ref_tokens.is_empty() || hyp_tokens.is_empty() {
        return 0.0;
    }

    let mut log_avg = 0.0;
    let mut all_nonzero = true;

    for n in 1..=4 {
        let precision = modified_precision(&ref_tokens, &hyp_tokens, n);
        if precision == 0.0 {
            all_nonzero = false;
            break;
        }
        log_avg += precision.ln();
    }

    if !all_nonzero {
        return 0.0;
    }

    log_avg /= 4.0;

    let bp = brevity_penalty(ref_tokens.len(), hyp_tokens.len());
    bp * log_avg.exp()
}

fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|s| s.to_lowercase())
        .collect()
}

fn ngrams(tokens: &[String], n: usize) -> HashMap<Vec<String>, usize> {
    let mut counts = HashMap::new();
    if tokens.len() < n {
        return counts;
    }
    for window in tokens.windows(n) {
        *counts.entry(window.to_vec()).or_insert(0) += 1;
    }
    counts
}

fn modified_precision(reference: &[String], hypothesis: &[String], n: usize) -> f64 {
    let ref_ngrams = ngrams(reference, n);
    let hyp_ngrams = ngrams(hypothesis, n);

    let mut clipped_count = 0usize;
    let mut total_count = 0usize;

    for (ngram, &hyp_count) in &hyp_ngrams {
        let ref_count = ref_ngrams.get(ngram).copied().unwrap_or(0);
        clipped_count += hyp_count.min(ref_count);
        total_count += hyp_count;
    }

    if total_count == 0 {
        return 0.0;
    }

    clipped_count as f64 / total_count as f64
}

fn brevity_penalty(ref_len: usize, hyp_len: usize) -> f64 {
    if hyp_len >= ref_len {
        1.0
    } else if hyp_len == 0 {
        0.0
    } else {
        (1.0 - ref_len as f64 / hyp_len as f64).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical() {
        let text = "the cat sat on the mat";
        let score = bleu4(text, text);
        assert!((score - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_completely_different() {
        assert_eq!(bleu4("the cat sat on the mat", "xyz abc def ghi jkl"), 0.0);
    }

    #[test]
    fn test_empty() {
        assert_eq!(bleu4("", "hello"), 0.0);
        assert_eq!(bleu4("hello", ""), 0.0);
        assert_eq!(bleu4("", ""), 0.0);
    }

    #[test]
    fn test_partial_overlap() {
        // Longer sentences needed for BLEU-4 to find 4-gram overlaps
        let reference = "the quick brown fox jumps over the lazy dog in the park";
        let hypothesis = "the quick brown cat jumps over the lazy dog in the garden";
        let score = bleu4(reference, hypothesis);
        assert!(score > 0.0 && score < 1.0, "Expected partial score, got {}", score);
    }

    #[test]
    fn test_brevity_penalty() {
        let bp = brevity_penalty(10, 10);
        assert_eq!(bp, 1.0);

        let bp = brevity_penalty(10, 5);
        assert!(bp < 1.0);

        let bp = brevity_penalty(10, 15);
        assert_eq!(bp, 1.0);
    }
}
