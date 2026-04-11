//! Chunk quality filtering for the distill indexing pipeline.
//!
//! Detects low-quality chunks that would pollute the vector space:
//! repetition loops, garbled text, or near-empty chunks.

use std::collections::HashMap;

/// Returns `true` if the chunk is too low quality to index.
pub fn is_low_quality(text: &str) -> bool {
    let trimmed = text.trim();

    // Too short to be useful
    let non_ws: usize = trimmed.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws < 50 {
        return true;
    }

    // Character repetition: any single char is >30% of the text
    if has_dominant_char(trimmed, 0.30) {
        return true;
    }

    // Word-level repetition: low vocabulary diversity
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 20 {
        let unique: std::collections::HashSet<&str> = words.iter().copied().collect();
        let diversity = unique.len() as f64 / words.len() as f64;
        if diversity < 0.10 {
            return true;
        }
    }

    // N-gram repetition: any 4-gram is >50% of the chunk's 4-grams
    if has_dominant_ngram(&words, 4, 0.50) {
        return true;
    }

    false
}

/// Check if any single character makes up more than `threshold` fraction of the text.
fn has_dominant_char(text: &str, threshold: f64) -> bool {
    let total = text.len();
    if total == 0 {
        return false;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    for ch in text.chars() {
        if !ch.is_whitespace() {
            *counts.entry(ch).or_default() += 1;
        }
    }
    counts
        .values()
        .any(|&count| (count as f64 / total as f64) > threshold)
}

/// Check if any word n-gram dominates the chunk.
fn has_dominant_ngram(words: &[&str], n: usize, threshold: f64) -> bool {
    if words.len() < n * 2 {
        return false;
    }
    let total_ngrams = words.len().saturating_sub(n - 1);
    if total_ngrams == 0 {
        return false;
    }
    let mut counts: HashMap<Vec<&str>, usize> = HashMap::new();
    for window in words.windows(n) {
        *counts.entry(window.to_vec()).or_default() += 1;
    }
    counts
        .values()
        .any(|&count| (count as f64 / total_ngrams as f64) > threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_chunk_passes() {
        let text = "The transformer architecture uses self-attention mechanisms \
                    to process sequences in parallel. This approach has proven \
                    highly effective for natural language processing tasks across \
                    many domains and languages.";
        assert!(!is_low_quality(text));
    }

    #[test]
    fn short_chunk_rejected() {
        assert!(is_low_quality("Hello world"));
        assert!(is_low_quality("   "));
        assert!(is_low_quality(""));
    }

    #[test]
    fn char_repetition_rejected() {
        let text = "ggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
        assert!(is_low_quality(text));
    }

    #[test]
    fn word_repetition_rejected() {
        let text = "and modeling and modeling and modeling and modeling and modeling \
                    and modeling and modeling and modeling and modeling and modeling \
                    and modeling and modeling and modeling and modeling and modeling";
        assert!(is_low_quality(text));
    }

    #[test]
    fn low_diversity_rejected() {
        // "J J J J J J ..." — very low vocabulary
        let text = std::iter::repeat_n("J", 100).collect::<Vec<_>>().join(" ");
        assert!(is_low_quality(&text));
    }

    #[test]
    fn latex_repetition_rejected() {
        let text = "$ and $ lines $ and $ lines $ and $ lines $ and $ lines $ and $ lines $ and $ lines $ and $ lines $ and $ lines $ and $ lines";
        assert!(is_low_quality(text));
    }
}
