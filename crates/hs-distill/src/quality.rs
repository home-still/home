//! Chunk quality filtering for the distill indexing pipeline.
//!
//! Detects low-quality chunks that would pollute the vector space:
//! repetition loops, garbled text, or near-empty chunks.

use std::collections::HashMap;

/// Why the quality filter rejected a chunk. Returned by [`explain`].
#[derive(Debug, Clone)]
pub enum RejectReason {
    TooShort { non_ws: usize },
    DominantChar { ch: char, ratio: f64 },
    LowDiversity { unique: usize, total: usize },
    DominantNgram { ratio: f64, n: usize },
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { non_ws } => {
                write!(f, "too short ({non_ws} non-ws chars < 50)")
            }
            Self::DominantChar { ch, ratio } => {
                let display: String = if ch.is_control() || *ch == ' ' {
                    format!("{:?}", ch)
                } else {
                    ch.to_string()
                };
                write!(
                    f,
                    "char {} is {:.1}% of text (> 30%)",
                    display,
                    ratio * 100.0
                )
            }
            Self::LowDiversity { unique, total } => {
                let pct = *unique as f64 / *total as f64 * 100.0;
                write!(f, "vocab diversity {unique}/{total} = {pct:.1}% (< 10%)",)
            }
            Self::DominantNgram { ratio, n } => {
                write!(
                    f,
                    "dominant {n}-gram is {:.1}% of {n}-grams (> 50%)",
                    ratio * 100.0,
                )
            }
        }
    }
}

/// Returns `true` if the chunk is too low quality to index.
pub fn is_low_quality(text: &str) -> bool {
    explain(text).is_some()
}

/// Returns the reason a chunk would be rejected, or `None` if acceptable.
pub fn explain(text: &str) -> Option<RejectReason> {
    let trimmed = text.trim();

    let non_ws: usize = trimmed.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws < 50 {
        return Some(RejectReason::TooShort { non_ws });
    }

    if let Some((ch, ratio)) = dominant_char(trimmed, 0.30) {
        return Some(RejectReason::DominantChar { ch, ratio });
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() >= 20 {
        let unique: std::collections::HashSet<&str> = words.iter().copied().collect();
        let diversity = unique.len() as f64 / words.len() as f64;
        if diversity < 0.10 {
            return Some(RejectReason::LowDiversity {
                unique: unique.len(),
                total: words.len(),
            });
        }
    }

    if let Some(ratio) = dominant_ngram(&words, 4, 0.50) {
        return Some(RejectReason::DominantNgram { ratio, n: 4 });
    }

    None
}

/// If any single character makes up more than `threshold` fraction of the text,
/// return that character and its ratio. Whitespace is excluded from the count.
fn dominant_char(text: &str, threshold: f64) -> Option<(char, f64)> {
    let total = text.len();
    if total == 0 {
        return None;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    for ch in text.chars() {
        if !ch.is_whitespace() {
            *counts.entry(ch).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(ch, count)| (ch, count as f64 / total as f64))
        .find(|(_, ratio)| *ratio > threshold)
}

/// If any word n-gram makes up more than `threshold` fraction of the n-grams,
/// return the ratio.
fn dominant_ngram(words: &[&str], n: usize, threshold: f64) -> Option<f64> {
    if words.len() < n * 2 {
        return None;
    }
    let total_ngrams = words.len().saturating_sub(n - 1);
    if total_ngrams == 0 {
        return None;
    }
    let mut counts: HashMap<Vec<&str>, usize> = HashMap::new();
    for window in words.windows(n) {
        *counts.entry(window.to_vec()).or_default() += 1;
    }
    counts
        .into_values()
        .map(|count| count as f64 / total_ngrams as f64)
        .find(|ratio| *ratio > threshold)
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
