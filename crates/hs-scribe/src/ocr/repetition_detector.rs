//! Streaming repetition-loop detector for VLM output.
//!
//! Phase 1 shadow data ([2026-05-07-glm-ocr-repetition-loops-research.md])
//! confirmed the residual failure mode after correct per-region routing is
//! per-region VLM repetition loops on legitimate crops, not wide-bbox
//! mis-detection. Calibration on the 5 known-loop pages from the diag JSONL
//! showed ~80% are surface-level n-gram repetition with a tight cycle:
//!
//!   - 2-3-token induction-head loops:  "and relationship and relationship..."
//!   - 5-8-token verbatim chunk repeat: "Movement amplitudes (Supplementary
//!     Table 1). Movement amplitudes (Supplementary Table 1) did not differ..."
//!
//! v1 catches these with a sliding-window word n-gram counter. The pure-
//! paraphrase residual (Nougat-style logits-variance EMA territory) is left
//! to v2 — only justified if v1 leaves recovery rate below 70%.
//!
//! Detection thresholds are calibrated against the 5-page sample, not derived
//! from theory. They will produce some false positives on bibliography
//! regions; the postprocess QC's bibliography-multiplier handles that
//! gracefully and the alternative (raising thresholds high enough to never
//! FP on bibs) lets through too many real loops.

use std::collections::{HashMap, VecDeque};
use unicode_segmentation::UnicodeSegmentation;

/// Window cap (words) over which n-gram counts are computed.
///
/// 256 picked as a balance: long enough that a 5-token cycle has space to
/// fire 3+ times before being flushed; short enough that legitimate
/// long-range repetition (a recurring section header in body prose) does
/// not falsely accumulate.
pub const WINDOW_CAP_WORDS: usize = 256;

/// Minimum window fill before any check runs. Avoids firing on the first
/// few words of a region — every loop in the calibration corpus needed
/// 30+ words of preamble before establishing.
const MIN_WINDOW_FOR_CHECK: usize = 32;

/// Why the detector aborted a stream. Surfaces into diag JSONL so a
/// post-mortem can attribute aborts to specific failure modes without
/// re-running OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopReason {
    /// 2-gram count crossed threshold. Tight induction-head copying:
    /// "and relationship and relationship and relationship..."
    Bigram,
    /// 3-gram count crossed threshold. Slightly larger cycle, still
    /// surface-level: "and their relationship..."
    Trigram,
    /// 4..=6-gram count crossed threshold. Verbatim chunk repeats —
    /// model rewinds and replays a segment.
    NGram(u8),
}

impl std::fmt::Display for LoopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopReason::Bigram => write!(f, "bigram-cycle"),
            LoopReason::Trigram => write!(f, "trigram-cycle"),
            LoopReason::NGram(n) => write!(f, "{n}-gram-cycle"),
        }
    }
}

/// Error returned when the streaming detector aborts a VLM request. The
/// caller (event_watch / per-region executor) downcasts an `anyhow::Error`
/// to this type to distinguish a controlled abort from a transport/server
/// failure.
#[derive(Debug, thiserror::Error)]
#[error(
    "VLM repetition loop detected ({reason}); aborted at {bytes_at_abort} bytes of partial output"
)]
pub struct RepetitionLoopError {
    pub reason: LoopReason,
    pub partial_output: String,
    pub bytes_at_abort: usize,
}

/// Sliding-window word n-gram counter. Owned per VLM request — feed
/// streamed deltas as they arrive, call [`check`](Self::check) after each
/// feed, abort the request when it returns `Some`.
pub struct RepetitionDetector {
    /// Last `WINDOW_CAP_WORDS` words, lowercased + ASCII-folded for
    /// hashing. Original chunk text is owned by the caller — the detector
    /// only sees normalized words.
    window: VecDeque<String>,
    /// Cap; mutable so tests can shrink the window without affecting
    /// production behavior.
    cap: usize,
}

impl Default for RepetitionDetector {
    fn default() -> Self {
        Self::new(WINDOW_CAP_WORDS)
    }
}

impl RepetitionDetector {
    pub fn new(cap: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(cap),
            cap: cap.max(MIN_WINDOW_FOR_CHECK),
        }
    }

    /// Append the words extracted from `delta` to the ring buffer. Older
    /// words are popped off the front when the cap is exceeded so that
    /// counts stay bounded by [`WINDOW_CAP_WORDS`].
    pub fn feed(&mut self, delta: &str) {
        for w in delta.unicode_words() {
            let normalized = normalize_word(w);
            if normalized.is_empty() {
                continue;
            }
            self.window.push_back(normalized);
            while self.window.len() > self.cap {
                self.window.pop_front();
            }
        }
    }

    /// Return `Some(LoopReason)` when the current window contains a
    /// pathological repetition, otherwise `None`. Cheap: linear scan over
    /// the window for each n in 2..=6 (a few thousand HashMap operations
    /// on a 256-word window — lost in the noise vs. VLM forward-pass
    /// latency).
    ///
    /// Threshold rationale (calibrated against 5 known-loop pages):
    ///
    /// - **n=2, threshold 10:** "and relationship" ×40 in 80-word span
    ///   fires immediately. "of the" in legitimate prose tops out around
    ///   5-7 in any 256-word window.
    /// - **n=3, threshold 5:** "and their relationship" ×40 fires
    ///   immediately. Legitimate 3-gram top counts in academic prose
    ///   are typically 2-3 ("the present study").
    /// - **n=4..=6, threshold 3:** Verbatim chunk repeats need only 3
    ///   occurrences. Legitimate ≥4-gram repeats above count 2 are rare
    ///   outside reference lists, which the bibliography QC multiplier
    ///   handles separately.
    pub fn check(&self) -> Option<LoopReason> {
        if self.window.len() < MIN_WINDOW_FOR_CHECK {
            return None;
        }
        if max_ngram_count(&self.window, 2) >= 10 {
            return Some(LoopReason::Bigram);
        }
        if max_ngram_count(&self.window, 3) >= 5 {
            return Some(LoopReason::Trigram);
        }
        for n in 4u8..=6 {
            if max_ngram_count(&self.window, n as usize) >= 3 {
                return Some(LoopReason::NGram(n));
            }
        }
        None
    }

    /// Window length in normalized words. Surface for tests and for
    /// the calibration harness to sanity-check fill before assertions.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }
}

/// Normalize a word for n-gram hashing: lowercase, ASCII-fold what we can
/// (drop combining marks via [`UnicodeSegmentation`]), strip punctuation-
/// only fragments. Returns an empty string for tokens that should be
/// skipped (pure punctuation, control chars).
fn normalize_word(w: &str) -> String {
    let mut out = String::with_capacity(w.len());
    for ch in w.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

/// Largest count of any n-gram in the window. Single linear pass building
/// a hash map over the 1+(window_len - n) n-grams.
fn max_ngram_count(window: &VecDeque<String>, n: usize) -> u32 {
    if n == 0 || window.len() < n {
        return 0;
    }
    let mut counts: HashMap<Vec<&str>, u32> = HashMap::with_capacity(window.len());
    let words: Vec<&str> = window.iter().map(|s| s.as_str()).collect();
    let mut max = 0u32;
    for start in 0..=(words.len() - n) {
        let key: Vec<&str> = words[start..start + n].to_vec();
        let entry = counts.entry(key).or_insert(0);
        *entry += 1;
        if *entry > max {
            max = *entry;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector_with_cap(cap: usize) -> RepetitionDetector {
        RepetitionDetector::new(cap)
    }

    #[test]
    fn empty_window_does_not_fire() {
        let d = RepetitionDetector::default();
        assert_eq!(d.check(), None);
    }

    #[test]
    fn below_min_window_does_not_fire_even_on_loops() {
        let mut d = RepetitionDetector::default();
        // 16 words — below the 32-word minimum. Even pure repetition
        // shouldn't fire because we don't have enough context.
        for _ in 0..8 {
            d.feed("and relationship");
        }
        assert!(d.window_len() < MIN_WINDOW_FOR_CHECK);
        assert_eq!(d.check(), None);
    }

    #[test]
    fn bigram_loop_from_calibration_corpus_fires() {
        // Replays the 10.1037_a0014226 page-0 failure pattern.
        let mut d = RepetitionDetector::default();
        // 50 repetitions of "and relationship" = 100 words, easily over
        // both the 32-word minimum and the 10× bigram threshold.
        for _ in 0..50 {
            d.feed("and relationship ");
        }
        match d.check() {
            Some(LoopReason::Bigram) => {}
            other => panic!("expected Bigram, got {other:?}"),
        }
    }

    #[test]
    fn trigram_loop_from_calibration_corpus_fires() {
        // Replays the 10.1002_ejsp.2184 page-0 pattern: "and their
        // relationship and their relationship..."
        let mut d = RepetitionDetector::default();
        for _ in 0..15 {
            d.feed("and their relationship ");
        }
        // Bigram fires first (45 instances of "and their" >> 10), but
        // we accept Bigram OR Trigram — the failure mode is correctly
        // identified as a loop either way.
        let v = d.check().expect("should fire");
        assert!(
            matches!(v, LoopReason::Bigram | LoopReason::Trigram),
            "expected a loop verdict, got {v:?}"
        );
    }

    #[test]
    fn five_token_chunk_repeat_from_calibration_fires() {
        // Replays the 10.1093_cercor_bhz192 page-5 pattern: a literal
        // 5-word chunk that repeats verbatim, embedded in surrounding
        // paragraph text. Build the surrounding text first to ensure
        // the detector exercises the n=4..=6 branch (and not just n=2/3).
        let mut d = RepetitionDetector::default();
        // 30 words of varied prose preamble — establishes context, no
        // loops to catch.
        d.feed(
            "Participants reported that the visual hand task was easier \
             to perform under conditions of low visibility (Wilcoxon's \
             signed-rank test) showing the effect was statistically \
             significant in this sample of sixteen.",
        );
        // The looping 5-word chunk, three times across drifting prose.
        for _ in 0..3 {
            d.feed(
                "Movement amplitudes Supplementary Table One did not \
                 differ between attentional sets or visibility levels. ",
            );
        }
        // The harder threshold-set branches (NGram) is the expected
        // verdict here, but Trigram also catches it because "Movement
        // amplitudes Supplementary" repeats verbatim — accept either.
        let v = d.check().expect("should fire");
        assert!(
            matches!(v, LoopReason::Trigram | LoopReason::NGram(_)),
            "expected n>=3 verdict, got {v:?}"
        );
    }

    #[test]
    fn legitimate_academic_prose_does_not_fire() {
        // Pulled from a known-good page (10.1126_science.aac4716). No
        // pathological repetition. Detector must NOT fire.
        let mut d = RepetitionDetector::default();
        d.feed(
            "We present an experimental design that distinguishes the \
             contributions of three competing accounts of cooperation. \
             The first account predicts a positive correlation between \
             generosity and trust under conditions of repeated interaction. \
             The second account predicts the opposite pattern when the \
             reputational stakes are sufficiently high. The third account \
             predicts no main effect of trust on generosity, but a strong \
             interaction with the partner's prior contribution. We \
             collected data from 240 participants across four sessions \
             and analyzed the results using a mixed-effects model with \
             random intercepts at the participant and session level.",
        );
        assert_eq!(
            d.check(),
            None,
            "detector false-positived on legitimate academic prose"
        );
    }

    #[test]
    fn long_passage_with_recurring_topic_does_not_fire() {
        // Stronger control: a passage that uses "model" and "study" and
        // "data" many times legitimately — exactly the kind of word-
        // density that could trip a poorly-calibrated bigram threshold.
        let mut d = RepetitionDetector::default();
        d.feed(
            "Our model assumes the data are independent. The study collected \
             survey data from a non-clinical sample. We fit the model to \
             the data using maximum likelihood. The data showed a positive \
             trend. Our second study replicated the model on a different \
             data set. Across both studies the model fit the data well. \
             Limitations of the model include the assumption that the data \
             are normally distributed. Future studies could relax this \
             assumption and test alternative model specifications. The \
             model parameters estimated from the data are reported in \
             Table 1, alongside parameter estimates from prior studies \
             that used similar model specifications on different samples.",
        );
        assert_eq!(d.check(), None, "false-positived on dense academic prose");
    }

    #[test]
    fn punctuation_only_tokens_are_dropped() {
        let mut d = RepetitionDetector::default();
        // Pure punctuation + whitespace — should add zero words.
        d.feed("...   ;;; --- ---");
        assert_eq!(d.window_len(), 0);
        assert_eq!(d.check(), None);
    }

    #[test]
    fn case_and_punctuation_are_normalized_for_hashing() {
        // "the model" and "The Model." should hash to the same n-gram.
        let mut d = RepetitionDetector::default();
        let prefix = "lorem ipsum dolor sit amet consectetur adipiscing elit \
                      sed do eiusmod tempor incididunt ut labore et dolore \
                      magna aliqua ut enim ad minim veniam quis nostrud ";
        d.feed(prefix);
        // Exactly 10 instances → n=2 threshold (10) at the boundary.
        for _ in 0..10 {
            d.feed("The Model. the MODEL ");
        }
        match d.check() {
            Some(LoopReason::Bigram) => {}
            other => panic!("normalized 2-gram should trip bigram, got {other:?}"),
        }
    }

    #[test]
    fn window_truncates_at_cap() {
        let mut d = detector_with_cap(64);
        for _ in 0..200 {
            d.feed("alpha ");
        }
        assert_eq!(d.window_len(), 64);
    }

    #[test]
    fn loop_outside_window_does_not_persist() {
        // Loop in the prefix that's been pushed out by later prose
        // should NOT trip the detector.
        let mut d = detector_with_cap(64);
        for _ in 0..40 {
            d.feed("and relationship ");
        }
        assert!(matches!(
            d.check(),
            Some(LoopReason::Bigram | LoopReason::Trigram | LoopReason::NGram(_))
        ));
        // Now flush the window with diverse prose.
        d.feed(
            "We present an experimental design that distinguishes the \
             contributions of three competing accounts of cooperation. \
             The first account predicts a positive correlation between \
             generosity and trust under conditions of repeated interaction. \
             The second account predicts the opposite pattern when the \
             reputational stakes are sufficiently high. The third account \
             predicts no main effect of trust on generosity. We collected \
             data from a hundred participants across four sessions and \
             analyzed the results using a mixed-effects model.",
        );
        assert_eq!(d.window_len(), 64);
        assert_eq!(d.check(), None, "old loop should have flushed out");
    }

    #[test]
    fn max_ngram_count_handles_sub_n_window() {
        let window: VecDeque<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(max_ngram_count(&window, 5), 0);
    }

    #[test]
    fn loop_reason_displays_descriptively() {
        assert_eq!(format!("{}", LoopReason::Bigram), "bigram-cycle");
        assert_eq!(format!("{}", LoopReason::Trigram), "trigram-cycle");
        assert_eq!(format!("{}", LoopReason::NGram(5)), "5-gram-cycle");
    }
}
