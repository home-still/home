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
//! regions. An aborted region is a hole in the markdown, so the caller counts
//! it as a skipped region and QC rejects the conversion as gapped (the tier
//! chain escalates); the alternative (raising thresholds high enough to
//! never FP on bibs) lets through too many real loops.

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

/// Longest n-gram the detector counts.
const MAX_NGRAM: usize = 6;
/// An n-gram is a fixed-size array of interned word ids, zero-padded past
/// `n` — hashing and comparing it touches no heap.
type NGramKey = [u32; MAX_NGRAM];

/// Occurrence counts of the n-grams of one length currently in the window,
/// plus how many distinct n-grams are at or over the loop threshold.
struct NGramCounter {
    n: usize,
    threshold: u32,
    counts: HashMap<NGramKey, u32>,
    /// Distinct n-grams whose count is `>= threshold`. The window holds a
    /// pathological repetition of this length iff this is non-zero.
    over: usize,
}

impl NGramCounter {
    fn new(n: usize, threshold: u32, capacity: usize) -> Self {
        Self {
            n,
            threshold,
            // An n-gram occupies one slot per window position, so the map
            // never needs to grow past the window size.
            counts: HashMap::with_capacity(capacity + 1),
            over: 0,
        }
    }

    fn key(&self, window: &VecDeque<u32>, start: usize) -> NGramKey {
        let mut key = [0u32; MAX_NGRAM];
        for (i, slot) in key.iter_mut().enumerate().take(self.n) {
            *slot = window[start + i];
        }
        key
    }

    /// Count the n-gram that now ends the window (if the window holds one).
    fn added(&mut self, window: &VecDeque<u32>) {
        let len = window.len();
        if len < self.n {
            return;
        }
        let key = self.key(window, len - self.n);
        let count = self.counts.entry(key).or_insert(0);
        *count += 1;
        if *count == self.threshold {
            self.over += 1;
        }
    }

    /// Un-count the n-gram that starts the window; call before popping it.
    fn evicting(&mut self, window: &VecDeque<u32>) {
        if window.len() < self.n {
            return;
        }
        let key = self.key(window, 0);
        if let Some(count) = self.counts.get_mut(&key) {
            if *count == self.threshold {
                self.over -= 1;
            }
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&key);
            }
        }
    }
}

/// Sliding-window word n-gram counter. Owned per VLM request — feed
/// streamed deltas as they arrive, call [`check`](Self::check) after each
/// feed, abort the request when it returns `Some`.
///
/// The window's n-gram counts are maintained incrementally: a word entering
/// or leaving the window adds or removes exactly one n-gram per length, and
/// each word is interned to a `u32` once. `feed` is O(1) per word and
/// `check` is O(1), with no heap allocation per n-gram (a pass over the
/// window used to allocate a `Vec<&str>` key per n-gram, ~10^7 times for an
/// 8k-token region, on the async runtime).
pub struct RepetitionDetector {
    /// Last `cap` words as interned ids of the lowercased, alphanumeric-only
    /// form. Original chunk text is owned by the caller — the detector only
    /// sees normalized words.
    window: VecDeque<u32>,
    /// Normalized word -> id. Ids start at 1 (0 is the key padding).
    ids: HashMap<String, u32>,
    next_id: u32,
    /// Reused buffer the current word is normalized into.
    scratch: String,
    /// Counters for n = 2..=MAX_NGRAM, index `n - 2`.
    counters: Vec<NGramCounter>,
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
        let cap = cap.max(MIN_WINDOW_FOR_CHECK);
        // Thresholds per length — see `check` for the calibration.
        let counters = (2..=MAX_NGRAM)
            .map(|n| {
                let threshold = match n {
                    2 => 10,
                    3 => 5,
                    _ => 3,
                };
                NGramCounter::new(n, threshold, cap)
            })
            .collect();
        Self {
            window: VecDeque::with_capacity(cap + 1),
            ids: HashMap::new(),
            next_id: 1,
            scratch: String::new(),
            counters,
            cap,
        }
    }

    /// Append the words extracted from `delta` to the ring buffer. Older
    /// words are popped off the front when the cap is exceeded so that
    /// counts stay bounded by [`WINDOW_CAP_WORDS`].
    pub fn feed(&mut self, delta: &str) {
        for w in delta.unicode_words() {
            normalize_word_into(w, &mut self.scratch);
            if self.scratch.is_empty() {
                continue;
            }
            let id = match self.ids.get(self.scratch.as_str()) {
                Some(&id) => id,
                None => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(self.scratch.clone(), id);
                    id
                }
            };
            self.window.push_back(id);
            for counter in &mut self.counters {
                counter.added(&self.window);
            }
            while self.window.len() > self.cap {
                for counter in &mut self.counters {
                    counter.evicting(&self.window);
                }
                self.window.pop_front();
            }
        }
    }

    /// Return `Some(LoopReason)` when the current window contains a
    /// pathological repetition, otherwise `None`. O(1): the per-length
    /// counters are kept current by [`feed`](Self::feed).
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
    ///   outside reference lists, where this can abort a legitimate region
    ///   (the conversion is then rejected as gapped and escalates).
    pub fn check(&self) -> Option<LoopReason> {
        if self.window.len() < MIN_WINDOW_FOR_CHECK {
            return None;
        }
        // Shortest cycle first: the same input trips the bigram rule before
        // the trigram rule, and so on.
        for counter in &self.counters {
            if counter.over > 0 {
                return Some(match counter.n {
                    2 => LoopReason::Bigram,
                    3 => LoopReason::Trigram,
                    n => LoopReason::NGram(n as u8),
                });
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

/// Normalize a word for n-gram hashing into `out` (cleared first): lowercase
/// and strip everything that is not alphanumeric. Leaves `out` empty for
/// tokens that should be skipped (pure punctuation, control chars).
fn normalize_word_into(w: &str, out: &mut String) {
    out.clear();
    for ch in w.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        }
    }
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
    fn loop_reason_displays_descriptively() {
        assert_eq!(format!("{}", LoopReason::Bigram), "bigram-cycle");
        assert_eq!(format!("{}", LoopReason::Trigram), "trigram-cycle");
        assert_eq!(format!("{}", LoopReason::NGram(5)), "5-gram-cycle");
    }

    // ── incremental counting vs. the original full-window scan ─────────

    /// The detector's original implementation: re-count every n-gram of the
    /// window on each check, keyed by a heap `Vec<&str>`. Kept as the oracle
    /// the incremental counters must agree with on every step.
    fn naive_max_ngram_count(window: &VecDeque<String>, n: usize) -> u32 {
        if n == 0 || window.len() < n {
            return 0;
        }
        let words: Vec<&str> = window.iter().map(|s| s.as_str()).collect();
        let mut counts: HashMap<Vec<&str>, u32> = HashMap::new();
        let mut max = 0u32;
        for start in 0..=(words.len() - n) {
            let c = counts.entry(words[start..start + n].to_vec()).or_insert(0);
            *c += 1;
            max = max.max(*c);
        }
        max
    }

    fn naive_check(window: &VecDeque<String>) -> Option<LoopReason> {
        if window.len() < MIN_WINDOW_FOR_CHECK {
            return None;
        }
        if naive_max_ngram_count(window, 2) >= 10 {
            return Some(LoopReason::Bigram);
        }
        if naive_max_ngram_count(window, 3) >= 5 {
            return Some(LoopReason::Trigram);
        }
        for n in 4u8..=6 {
            if naive_max_ngram_count(window, n as usize) >= 3 {
                return Some(LoopReason::NGram(n));
            }
        }
        None
    }

    /// Words drawn from a small vocabulary so that repeats, cycles and
    /// evictions all occur; deterministic LCG, no dev-dependency.
    fn pseudo_words(seed: u64, count: usize, vocab: u64) -> Vec<String> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                format!("w{}", (state >> 33) % vocab)
            })
            .collect()
    }

    #[test]
    fn incremental_counters_agree_with_the_full_window_scan_at_every_step() {
        for (seed, vocab, cap) in [
            (1, 3, 64),
            (2, 6, 64),
            (3, 12, 64),
            (4, 40, 256),
            (5, 2, 256),
        ] {
            let mut d = detector_with_cap(cap);
            let mut oracle: VecDeque<String> = VecDeque::new();
            for (step, word) in pseudo_words(seed, 1500, vocab).into_iter().enumerate() {
                d.feed(&format!("{word} "));
                oracle.push_back(word);
                while oracle.len() > d.cap {
                    oracle.pop_front();
                }
                assert_eq!(
                    d.check(),
                    naive_check(&oracle),
                    "seed {seed} vocab {vocab} cap {cap} step {step}"
                );
            }
        }
    }

    #[test]
    fn a_loop_that_ends_stops_firing_once_it_has_left_the_window() {
        // 2-word cycle fires; after a full window of non-repeating words the
        // counters must have fully drained (no stuck `over`).
        let mut d = detector_with_cap(64);
        for _ in 0..40 {
            d.feed("and relationship ");
        }
        assert!(d.check().is_some());
        for i in 0..200 {
            d.feed(&format!("unique{i} "));
        }
        assert_eq!(d.check(), None);
        assert!(d.counters.iter().all(|c| c.over == 0));
        // Every window position holds at most one n-gram per length, and
        // all of them are distinct now.
        assert!(d
            .counters
            .iter()
            .all(|c| c.counts.values().all(|&v| v == 1)));
    }

    // ── no heap work per n-gram ────────────────────────────────────────

    mod alloc_count {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static ALLOCS: Cell<usize> = const { Cell::new(0) };
        }

        pub struct Counting;

        // SAFETY: delegates to the system allocator; only a thread-local
        // counter is touched on the side.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
                System.alloc(layout)
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                System.dealloc(ptr, layout)
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
                System.realloc(ptr, layout, new_size)
            }
        }

        pub fn count<R>(f: impl FnOnce() -> R) -> (R, usize) {
            let before = ALLOCS.with(|c| c.get());
            let out = f();
            (out, ALLOCS.with(|c| c.get()) - before)
        }
    }

    #[global_allocator]
    static COUNTING: alloc_count::Counting = alloc_count::Counting;

    #[test]
    fn steady_state_feed_and_check_allocate_nothing() {
        // The old check() built a heap Vec<&str> key per n-gram per length
        // on every delta. With the window full and its vocabulary interned,
        // feeding words and checking must not touch the allocator at all.
        let vocab: Vec<String> = (0..40).map(|i| format!("word{i}")).collect();
        let mut d = RepetitionDetector::default();
        let cycle = |d: &mut RepetitionDetector, rounds: usize| {
            for r in 0..rounds {
                for i in 0..vocab.len() {
                    // Vary the order so n-grams keep appearing and leaving.
                    d.feed(&vocab[(i * 7 + r) % vocab.len()]);
                    let _ = d.check();
                }
            }
        };
        cycle(&mut d, 20); // warm-up: fills the window, interns the vocabulary
        assert_eq!(d.window_len(), WINDOW_CAP_WORDS);
        let ((), allocs) = alloc_count::count(|| cycle(&mut d, 40));
        assert_eq!(allocs, 0, "feed/check allocated in steady state");
    }
}
