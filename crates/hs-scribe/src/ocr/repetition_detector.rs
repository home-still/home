//! Streaming repetition-loop detector for VLM output.
//!
//! A degenerate VLM loop repeats the same unit **back to back** until the
//! token limit: "and relationship and relationship and relationship…", or
//! the last sentence over and over. Two findings decide what to detect:
//!
//! - *Learning to Break the Loop* (Xu et al., 2022): models get stuck in
//!   consecutive repetitions, while consecutive sentence-level repetition is
//!   ~0.02 % of human text (Wikitext-103); and repetition is
//!   self-reinforcing — the more copies, the likelier the next. A loop that
//!   has started does not end by itself.
//! - *Nougat* (Blecher et al., 2023), on academic-PDF OCR: "the last
//!   sentence or paragraph is repeated over and over again" (1.5 % of pages).
//!
//! So the detector fires only on **consecutive periodicity**: the stream's
//! tail is at least [`MIN_REPEATS`] copies of one unit of `p` words
//! (`p <= MAX_PERIOD`), spanning at least [`MIN_LOOP_WORDS`] words.
//!
//! It used to count n-grams anywhere in a 256-word window (a 4–6-gram three
//! times, a trigram five times, a bigram ten times). Human text does that
//! all the time — affiliation lists, citation runs ("et al., 2008;" ten
//! times), a results paragraph saying "the number of choices" three times.
//! Run over 200 accepted documents, that rule aborted at least one region
//! in 96 % of the olmocr-converted ones (text it never vetted), and since an
//! aborted region rejects the whole conversion as gapped, every document of
//! the 24 h before the change was permanently dropped. Waiting for an
//! unambiguous signal is cheap here: a real loop never stops, so firing a
//! few dozen words later costs only those tokens, while a false positive
//! costs the document.
//!
//! Not caught: paraphrase loops (near-copies with words changed), which
//! Nougat notes defeat any strict detector. They run to the token limit and
//! the post-processing cleaner handles them, as before this detector.

use std::hash::{DefaultHasher, Hasher};
use unicode_segmentation::UnicodeSegmentation;

/// Longest repeating unit, in words, the detector recognizes: a long
/// paragraph. Three copies of it is the most a loop runs before firing.
pub const MAX_PERIOD: usize = 128;

/// Consecutive copies of a unit that make a loop.
pub const MIN_REPEATS: usize = 3;

/// Shortest repeated tail, in words, that makes a loop. Keeps short
/// legitimate repetition ("very very very", a row of three zeros) from
/// firing; a real two-word loop reaches it within a sentence's worth of
/// tokens.
pub const MIN_LOOP_WORDS: usize = 32;

/// Why the detector aborted a stream: the length of the repeating unit.
/// Surfaces into diag JSONL so a post-mortem can attribute aborts without
/// re-running OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopReason {
    /// A two-word unit repeated: "and relationship and relationship…".
    Bigram,
    /// A three-word unit repeated: "and their relationship and their…".
    Trigram,
    /// A unit of this many words repeated (1, or 4..=[`MAX_PERIOD`]) — a
    /// phrase, a sentence or a paragraph replayed verbatim.
    NGram(u8),
}

impl LoopReason {
    fn of_period(p: usize) -> Self {
        match p {
            2 => LoopReason::Bigram,
            3 => LoopReason::Trigram,
            n => LoopReason::NGram(n as u8),
        }
    }
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

/// Streaming loop detector. Owned per VLM request — feed streamed deltas as
/// they arrive, call [`check`](Self::check) after each feed, abort the
/// request when it returns `Some`.
///
/// Per word: the word's hash is compared with the word `p` back for every
/// period `p`, extending or resetting that period's run. The tail is `p`-
/// periodic over `run[p] + p` words. O([`MAX_PERIOD`]) per word, O(MAX_PERIOD)
/// per check, fixed-size state, no heap allocation.
pub struct RepetitionDetector {
    /// The last [`MAX_PERIOD`] word hashes, indexed by word count modulo
    /// `MAX_PERIOD`.
    ring: [u64; MAX_PERIOD],
    /// Words fed so far.
    words: usize,
    /// `run[p]`: how many consecutive words, ending at the newest, equal the
    /// word `p` before them.
    run: [u32; MAX_PERIOD + 1],
}

impl Default for RepetitionDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl RepetitionDetector {
    pub fn new() -> Self {
        Self {
            ring: [0; MAX_PERIOD],
            words: 0,
            run: [0; MAX_PERIOD + 1],
        }
    }

    /// Add the words in `delta` (case- and punctuation-insensitive; tokens
    /// with no letter or digit are skipped).
    pub fn feed(&mut self, delta: &str) {
        for w in delta.unicode_words() {
            let Some(hash) = word_hash(w) else {
                continue;
            };
            for p in 1..=MAX_PERIOD {
                if self.words >= p && self.ring[(self.words - p) % MAX_PERIOD] == hash {
                    self.run[p] = self.run[p].saturating_add(1);
                } else {
                    self.run[p] = 0;
                }
            }
            self.ring[self.words % MAX_PERIOD] = hash;
            self.words += 1;
        }
    }

    /// `Some` when the tail of the stream is a loop: at least
    /// [`MIN_REPEATS`] consecutive copies of one unit, spanning at least
    /// [`MIN_LOOP_WORDS`] words. The shortest such unit is reported.
    pub fn check(&self) -> Option<LoopReason> {
        (1..=MAX_PERIOD)
            .find(|&p| {
                let periodic = self.run[p] as usize + p;
                periodic >= MIN_LOOP_WORDS.max(MIN_REPEATS * p)
            })
            .map(LoopReason::of_period)
    }
}

/// Hash of a word's lowercased letters and digits; `None` for a token with
/// none (pure punctuation). Hashes as it normalizes, so no string is built.
fn word_hash(w: &str) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    let mut any = false;
    for ch in w.chars().filter(|c| c.is_alphanumeric()) {
        for lower in ch.to_lowercase() {
            hasher.write_u32(lower as u32);
            any = true;
        }
    }
    any.then(|| hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(text: &str) -> RepetitionDetector {
        let mut d = RepetitionDetector::default();
        d.feed(text);
        d
    }

    /// Feed `text` word by word, checking after each, as the streaming path
    /// does; the first verdict, if any.
    fn first_verdict(text: &str) -> Option<LoopReason> {
        let mut d = RepetitionDetector::default();
        for w in text.split_whitespace() {
            d.feed(w);
            d.feed(" ");
            if let Some(r) = d.check() {
                return Some(r);
            }
        }
        None
    }

    #[test]
    fn an_empty_stream_does_not_fire() {
        assert_eq!(RepetitionDetector::default().check(), None);
    }

    // ── loops it must catch (the calibration corpus's failure patterns) ──

    #[test]
    fn a_two_word_loop_fires_as_bigram() {
        // 10.1037_a0014226 page 0.
        assert_eq!(
            first_verdict(&"and relationship ".repeat(50)),
            Some(LoopReason::Bigram)
        );
    }

    #[test]
    fn a_three_word_loop_fires_as_trigram() {
        // 10.1002_ejsp.2184 page 0.
        assert_eq!(
            first_verdict(&"and their relationship ".repeat(15)),
            Some(LoopReason::Trigram)
        );
    }

    #[test]
    fn a_sentence_replayed_back_to_back_fires_at_its_third_copy() {
        // 10.1093_cercor_bhz192 page 5: a 14-word sentence replayed.
        let sentence = "Movement amplitudes Supplementary Table One did not \
                        differ between attentional sets or visibility levels. ";
        let preamble = "Participants reported that the visual hand task was easier \
                        to perform under conditions of low visibility. ";
        assert_eq!(
            first_verdict(&format!("{preamble}{}", sentence.repeat(2))),
            None
        );
        assert_eq!(
            first_verdict(&format!("{preamble}{}", sentence.repeat(3))),
            Some(LoopReason::NGram(14))
        );
    }

    #[test]
    fn case_and_punctuation_do_not_hide_a_loop() {
        let d = fed(&"The Model. the MODEL, the model; ".repeat(12));
        assert_eq!(d.check(), Some(LoopReason::Bigram));
        // Punctuation-only tokens between the words change nothing.
        let d = fed(&"and --- relationship ... ".repeat(30));
        assert_eq!(d.check(), Some(LoopReason::Bigram));
    }

    #[test]
    fn the_longest_period_fires_and_a_longer_one_does_not() {
        let paragraph = |n: usize| -> String { (0..n).map(|i| format!("word{i} ")).collect() };
        assert_eq!(
            fed(&paragraph(MAX_PERIOD).repeat(3)).check(),
            Some(LoopReason::NGram(MAX_PERIOD as u8))
        );
        // Longer units run to the token limit; the post-processing cleaner
        // handles them, as before this detector existed.
        assert_eq!(fed(&paragraph(MAX_PERIOD + 1).repeat(3)).check(), None);
    }

    // ── text it must leave alone ────────────────────────────────────────

    /// Each of these aborted a region under the old n-gram-count rule and
    /// is ordinary text from accepted documents (2026-10-23 measurement).
    #[test]
    fn recurring_phrases_in_real_prose_do_not_fire() {
        for passage in [
            // An affiliation list: the same place three times.
            "1Department of Psychiatry, University of California–San Diego, La Jolla, \
             California 92093-0109, 4Department of Psychology, University of \
             California–San Diego, La Jolla, California 92093-1090, 5Health Services \
             Research and Development, Veteran Affairs San Diego Healthcare System, La \
             Jolla, California 92161, and 6Kavli Foundation for the Brain and Mind, \
             University of California–San Diego, La Jolla, California 92093",
            // A citation run: "et al., 20xx;" over and over.
            "which has been linked to postsensory decision-related processes \
             (Hillyard et al., 1972; Squires et al., 1973, 1975; Mangun and Buck, \
             1998; O'Connell et al., 2012; Kelly and O'Connell, 2013; Itthipuripat \
             et al., 2014a, 2015, 2017; Twomey et al., 2015; Ho et al., 2012a,b; \
             Purcell et al., 2012; Wang, 2012; Ester et al., 2014; Keuken et al., 2015)",
            // A results paragraph repeating its key phrase.
            "Contrast thresholds also increased as the number of choices increased, \
             and response times increased as the number of choices increased. There \
             was no interaction between attention and the number of choices on \
             response times, and no main effect of the number of choices on the \
             maximum response.",
            // Dense topical prose.
            "Our model assumes the data are independent. The study collected survey \
             data from a non-clinical sample. We fit the model to the data using \
             maximum likelihood. The data showed a positive trend. Our second study \
             replicated the model on a different data set. Across both studies the \
             model fit the data well.",
        ] {
            assert_eq!(first_verdict(passage), None, "fired on: {passage}");
        }
    }

    #[test]
    fn short_legitimate_repetition_does_not_fire() {
        assert_eq!(first_verdict("it was very very very good indeed"), None);
        assert_eq!(
            first_verdict("scores were 0 0 0 0 0 0 in the control arm"),
            None
        );
        // A phrase duplicated once — a glitch, not a loop — is left alone.
        let once = "Movement amplitudes Supplementary Table One. Movement amplitudes \
                    Supplementary Table One did not differ between attentional sets.";
        assert_eq!(first_verdict(once), None);
    }

    #[test]
    fn a_loop_that_ends_stops_firing() {
        let mut d = fed(&"and relationship ".repeat(40));
        assert!(d.check().is_some());
        d.feed("the authors then turned to a different question entirely");
        assert_eq!(d.check(), None);
    }

    #[test]
    fn loop_reason_displays_descriptively() {
        assert_eq!(LoopReason::Bigram.to_string(), "bigram-cycle");
        assert_eq!(LoopReason::Trigram.to_string(), "trigram-cycle");
        assert_eq!(LoopReason::NGram(14).to_string(), "14-gram-cycle");
    }

    // ── the incremental runs against a direct scan of the whole stream ──

    /// The rule stated directly: the shortest period `p` whose tail of
    /// identical-every-`p` words reaches the loop length.
    fn naive_check(words: &[String]) -> Option<LoopReason> {
        let n = words.len();
        (1..=MAX_PERIOD.min(n)).find_map(|p| {
            let mut periodic = p;
            while periodic < n && words[n - 1 - (periodic - p)] == words[n - 1 - periodic] {
                periodic += 1;
            }
            (periodic >= MIN_LOOP_WORDS.max(MIN_REPEATS * p)).then(|| LoopReason::of_period(p))
        })
    }

    /// Small-vocabulary words with injected loops of assorted periods, so
    /// runs start, grow, break and fire; deterministic LCG.
    fn stream(seed: u64, count: usize, vocab: u64) -> Vec<String> {
        let mut state = seed;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        };
        let mut out: Vec<String> = Vec::new();
        while out.len() < count {
            if next() % 9 == 0 && !out.is_empty() {
                let p = (next() as usize % 20 + 1).min(out.len());
                let copies = next() as usize % 6 + 1;
                let unit: Vec<String> = out[out.len() - p..].to_vec();
                for _ in 0..copies {
                    out.extend(unit.iter().cloned());
                }
            } else {
                out.push(format!("w{}", next() % vocab));
            }
        }
        out
    }

    #[test]
    fn incremental_runs_agree_with_a_direct_scan_at_every_word() {
        for (seed, vocab) in [(1, 3), (2, 8), (3, 40), (4, 1000)] {
            let words = stream(seed, 3000, vocab);
            let mut d = RepetitionDetector::default();
            for (i, w) in words.iter().enumerate() {
                d.feed(w);
                assert_eq!(
                    d.check(),
                    naive_check(&words[..=i]),
                    "seed {seed} vocab {vocab} word {i}"
                );
            }
        }
    }

    // ── no heap work ─────────────────────────────────────────────────────

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
    fn feed_and_check_allocate_nothing() {
        let vocab: Vec<String> = (0..40).map(|i| format!("word{i}")).collect();
        let mut d = RepetitionDetector::default();
        let ((), allocs) = alloc_count::count(|| {
            for r in 0..60 {
                for i in 0..vocab.len() {
                    d.feed(&vocab[(i * 7 + r) % vocab.len()]);
                    let _ = d.check();
                }
            }
        });
        assert_eq!(allocs, 0, "feed/check allocated");
    }
}
