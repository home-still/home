//! Post-processing for scribe markdown output.
//!
//! Detects and truncates repetition loops that the VLM model produces,
//! such as "and modeling and modeling and modeling..." or "ggggggg...".

/// Grab up to `window` chars of `original` centered on the first byte
/// position where `original` and `cleaned` diverge. Used by the repetition
/// scanner to emit a short, human-readable sample of the offending run so
/// operators can eyeball whether the flag is a genuine loop or a false
/// positive (e.g. a reference list or DNA sequence that legitimately
/// repeats tokens).
pub fn divergence_snippet(original: &str, cleaned: &str, window: usize) -> Option<String> {
    if original == cleaned {
        return None;
    }
    let original_chars: Vec<(usize, char)> = original.char_indices().collect();
    let cleaned_chars: Vec<(usize, char)> = cleaned.char_indices().collect();

    let mut diverge_char_idx = original_chars.len().min(cleaned_chars.len());
    for (i, ((_, a), (_, b))) in original_chars.iter().zip(cleaned_chars.iter()).enumerate() {
        if a != b {
            diverge_char_idx = i;
            break;
        }
    }

    let half = window / 2;
    let start = diverge_char_idx.saturating_sub(half);
    let end = (diverge_char_idx + half).min(original_chars.len());

    let start_byte = original_chars.get(start).map(|(b, _)| *b).unwrap_or(0);
    let end_byte = original_chars
        .get(end)
        .map(|(b, _)| *b)
        .unwrap_or(original.len());

    Some(original[start_byte..end_byte].to_string())
}

/// Verdict from the post-processing QC gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QcVerdict {
    /// Markdown passed the repetition QC — safe to persist.
    Accept,
    /// Markdown contains too many repetition truncations to be trusted.
    /// Caller should stamp `conversion.failed` with `reason="repetition_loop"`
    /// and NOT write the markdown object.
    RejectLoop,
}

/// Absolute truncation ceiling: more than this across the whole doc is a
/// runaway VLM loop regardless of length.
const QC_ABSOLUTE_MAX: usize = 20;
/// Per-page truncation ceiling: catches papers that are long enough to
/// dilute the absolute ceiling but still have loopy content on some pages.
const QC_PER_PAGE_MAX: usize = 5;
/// Longest contiguous repeated-substring run, in bytes. The truncation-site
/// count alone misses single fat loops: one continuous 9 KB run of "the
/// retrieval of" collapses to a single site under `clean_repetitions` and
/// would slip through the count-based gate. 1 KB is well above any
/// legitimate repeat content (table separators, citation boilerplate).
const QC_LONGEST_RUN_BYTES_MAX: usize = 1024;
/// Repetition floor below which a run isn't considered loop-like.
/// Four-or-more consecutive repeats matches the strictest pass in
/// `clean_repetitions` (4-gram >3×).
const LOOP_MIN_REPS: usize = 4;

/// Decide whether the markdown that came out of `clean_repetitions` is
/// trustworthy. Trips on any of:
/// - absolute truncation count > `QC_ABSOLUTE_MAX`
/// - per-page truncation density > `QC_PER_PAGE_MAX`
/// - longest contiguous repeated-substring run > `QC_LONGEST_RUN_BYTES_MAX`
///
/// `longest_run_bytes` should be computed on the **original** (pre-cleanup)
/// markdown via [`longest_repeated_run_bytes`]. Without that signal a single
/// fat loop registers as one truncation site and slips both count gates.
pub fn qc_verdict(truncations: usize, total_pages: u64, longest_run_bytes: usize) -> QcVerdict {
    if truncations > QC_ABSOLUTE_MAX {
        return QcVerdict::RejectLoop;
    }
    let pages = total_pages.max(1) as usize;
    if truncations > pages.saturating_mul(QC_PER_PAGE_MAX) {
        return QcVerdict::RejectLoop;
    }
    if longest_run_bytes > QC_LONGEST_RUN_BYTES_MAX {
        return QcVerdict::RejectLoop;
    }
    QcVerdict::Accept
}

/// Longest contiguous repeated-substring run in the input, measured in
/// bytes. Considers character-level runs (matching `clean_char_repetitions`)
/// and word-n-gram-level runs for n in 1..=4 (matching the cleaning passes).
/// Only runs of at least `LOOP_MIN_REPS` consecutive repetitions are
/// counted; below that, repetition is consistent with normal prose
/// (citation lists, "and X and Y and Z").
///
/// Run with `longest_repeated_run_bytes(original)` *before*
/// `clean_repetitions` strips the run — once cleaned, the loop is gone and
/// the byte span is unrecoverable.
pub fn longest_repeated_run_bytes(text: &str) -> usize {
    let mut max_run = 0;

    // Character-level: walk the whole text. Char runs aren't constrained
    // to single lines (a `gggggg...` run can span newlines).
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start_byte, ch) = chars[i];
        let mut j = i + 1;
        while j < chars.len() && chars[j].1 == ch {
            j += 1;
        }
        let reps = j - i;
        if reps >= LOOP_MIN_REPS {
            let end_byte = chars.get(j).map(|(b, _)| *b).unwrap_or(text.len());
            max_run = max_run.max(end_byte - start_byte);
        }
        i = j;
    }

    // Word-n-gram level: process line by line — `clean_ngram_repetitions`
    // does the same. n=3 catches the F1 incident ("the retrieval of "
    // repeated); the 1..=4 sweep covers the rest of the cleaning passes.
    for line in text.split('\n') {
        let words = collect_word_positions(line);
        if words.is_empty() {
            continue;
        }
        for n in 1..=4 {
            if words.len() < n * LOOP_MIN_REPS {
                continue;
            }
            let mut i = 0;
            while i + n <= words.len() {
                let ngram: Vec<&str> = words[i..i + n].iter().map(|(_, w)| *w).collect();
                let mut reps = 1;
                let mut j = i + n;
                while j + n <= words.len()
                    && words[j..j + n]
                        .iter()
                        .map(|(_, w)| *w)
                        .eq(ngram.iter().copied())
                {
                    reps += 1;
                    j += n;
                }
                if reps >= LOOP_MIN_REPS {
                    let start_byte = words[i].0;
                    let (last_start, last_word) = words[j - 1];
                    let end_byte = last_start + last_word.len();
                    max_run = max_run.max(end_byte - start_byte);
                    i = j;
                } else {
                    i += 1;
                }
            }
        }
    }

    max_run
}

/// Tokenize `line` into (byte_offset, word) pairs without allocating per
/// word. Whitespace via `char::is_whitespace` so multi-byte separators
/// don't accidentally land inside a word.
fn collect_word_positions(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        let mut end = start + ch.len_utf8();
        chars.next();
        while let Some(&(_, c)) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            end += c.len_utf8();
            chars.next();
        }
        out.push((start, &line[start..end]));
    }
    out
}

/// Clean repetition artifacts from markdown text.
///
/// Returns `(cleaned_text, truncation_count)` where `truncation_count`
/// is the number of repetition sites that were truncated.
pub fn clean_repetitions(text: &str) -> (String, usize) {
    let mut result = text.to_string();
    let mut total_truncations = 0;

    // Pass 1: character-level repetition (>10 consecutive identical chars)
    let (cleaned, count) = clean_char_repetitions(&result);
    result = cleaned;
    total_truncations += count;

    // Pass 2: word n-gram repetition (4-gram repeating >3 consecutive times)
    let (cleaned, count) = clean_ngram_repetitions(&result, 4, 3);
    result = cleaned;
    total_truncations += count;

    // Pass 3: shorter n-grams (2-gram repeating >4 consecutive times)
    let (cleaned, count) = clean_ngram_repetitions(&result, 2, 4);
    result = cleaned;
    total_truncations += count;

    // Pass 4: unigram runs (same word repeating >5 consecutive times).
    // Catches VLM artifacts like "P, P, P, P, P" and ". . . . ." where
    // alternating tokens prevent the 2-gram pass from engaging.
    let (cleaned, count) = clean_ngram_repetitions(&result, 1, 5);
    result = cleaned;
    total_truncations += count;

    (result, total_truncations)
}

/// Remove runs of >threshold consecutive identical characters.
/// Keeps one occurrence of the character.
fn clean_char_repetitions(text: &str) -> (String, usize) {
    let threshold = 10;
    let mut result = String::with_capacity(text.len());
    let mut truncations = 0;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        result.push(ch);
        let mut run = 1;
        while chars.peek() == Some(&ch) {
            chars.next();
            run += 1;
        }
        if run > threshold {
            // Keep just one, we already pushed it
            truncations += 1;
        } else {
            // Push the remaining occurrences (run - 1, since we already pushed one)
            for _ in 0..run - 1 {
                result.push(ch);
            }
        }
    }

    (result, truncations)
}

/// Remove consecutive repetitions of word n-grams.
///
/// If an n-gram repeats more than `max_repeats` consecutive times,
/// keep only the first occurrence.
fn clean_ngram_repetitions(text: &str, n: usize, max_repeats: usize) -> (String, usize) {
    // Process line by line to preserve structure
    let mut result_lines = Vec::new();
    let mut truncations = 0;

    for line in text.split('\n') {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < n * 2 {
            result_lines.push(line.to_string());
            continue;
        }

        let mut cleaned_words: Vec<&str> = Vec::new();
        let mut i = 0;

        while i < words.len() {
            if i + n <= words.len() {
                let ngram: Vec<&str> = words[i..i + n].to_vec();

                // Count consecutive repetitions of this n-gram
                let mut repeat_count = 1;
                let mut j = i + n;
                while j + n <= words.len() && words[j..j + n] == ngram[..] {
                    repeat_count += 1;
                    j += n;
                }

                if repeat_count > max_repeats {
                    // Keep just one occurrence
                    cleaned_words.extend_from_slice(&ngram);
                    i = j; // skip all repetitions
                    truncations += 1;
                } else {
                    cleaned_words.push(words[i]);
                    i += 1;
                }
            } else {
                cleaned_words.push(words[i]);
                i += 1;
            }
        }

        result_lines.push(cleaned_words.join(" "));
    }

    (result_lines.join("\n"), truncations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_repetition_truncated() {
        let input = "hello ggggggggggggggggggg world";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, "hello g world");
        assert_eq!(count, 1);
    }

    #[test]
    fn ngram_repetition_truncated() {
        let input =
            "and modeling and modeling and modeling and modeling and modeling and modeling done";
        let (output, count) = clean_repetitions(input);
        assert!(output.contains("and modeling"));
        assert!(!output.contains("and modeling and modeling and modeling and modeling"));
        assert!(output.ends_with("done"));
        assert!(count > 0);
    }

    #[test]
    fn short_text_unchanged() {
        let input = "This is fine.";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count, 0);
    }

    #[test]
    fn normal_repetition_preserved() {
        // "the" appearing naturally should not be truncated
        let input = "the cat and the dog and the bird";
        let (output, _) = clean_repetitions(input);
        assert_eq!(output, input);
    }

    #[test]
    fn bigram_repetition_truncated() {
        let input = "J J J J J J J J J J J J J J done";
        let (output, count) = clean_repetitions(input);
        assert!(output.contains("J J"));
        assert!(output.ends_with("done"));
        assert!(count > 0);
    }

    #[test]
    fn multiline_preserved() {
        let input = "Line one\n\nLine two\n\n---\n\nLine three";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count, 0);
    }

    #[test]
    fn mixed_repetitions() {
        let input = "eseseseseseseseseseseseseses and the model the model the model the model the model the model end";
        let (output, count) = clean_repetitions(input);
        assert!(count >= 1); // "the model" bigram repetition is caught
        assert!(output.contains("end"));
        assert!(!output.contains("the model the model the model the model the model"));
    }

    #[test]
    fn unigram_repetition_truncated() {
        // From a real VLM loop on a child-protection paper's author list:
        // "Bywaters, Pover, P, P, P, P, P, Featherstone, B".
        // The `P,` unigram repeats — 2-gram and 4-gram passes miss it
        // because the surrounding tokens differ.
        let input = "Bywaters, Pover, P, P, P, P, P, P, P, P, Featherstone, B";
        let (output, count) = clean_repetitions(input);
        assert!(count > 0, "expected at least one truncation");
        assert!(
            !output.contains("P, P, P, P, P, P"),
            "unigram run should be collapsed: {output}"
        );
        assert!(output.ends_with("Featherstone, B"));
    }

    #[test]
    fn mixed_token_period_run_truncated() {
        // From a real VLM loop on a WASH paper: "s. s. s. . . . .".
        // Two unigrams alternate: `s.` and `.`. Each is a unigram run.
        let input = "sample s. s. s. s. s. s. s. s. . . . . . . end";
        let (output, _count) = clean_repetitions(input);
        assert!(
            !output.contains("s. s. s. s. s. s."),
            "`s.` unigram run should be collapsed: {output}"
        );
        assert!(
            !output.contains(". . . . ."),
            "`.` unigram run should be collapsed: {output}"
        );
        assert!(output.contains("end"));
    }

    #[test]
    fn qc_verdict_accepts_clean_doc() {
        assert_eq!(qc_verdict(0, 10, 0), QcVerdict::Accept);
        assert_eq!(qc_verdict(5, 10, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_absolute_runaway() {
        // 1-page doc with 21 truncations — clear loop.
        assert_eq!(qc_verdict(21, 1, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_per_page_runaway() {
        // 5-page doc with 30 truncations — over the per-page ceiling
        // (5 * 5 = 25) and also over the absolute ceiling (20).
        assert_eq!(qc_verdict(30, 5, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_long_docs() {
        // 30-page survey with 18 truncations — within both ceilings.
        assert_eq!(qc_verdict(18, 30, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_single_long_run() {
        // F1 incident shape: one 9.4 KB contiguous run of "the retrieval of"
        // collapses to a single truncation site. Count and per-page would
        // pass; longest-run gate must trip.
        assert_eq!(qc_verdict(1, 21, 9400), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_short_legitimate_repeats() {
        // Citation boilerplate / table separators stay below 1 KB and
        // shouldn't trip even when the count is 0.
        assert_eq!(qc_verdict(0, 10, 800), QcVerdict::Accept);
    }

    #[test]
    fn divergence_snippet_identical_returns_none() {
        assert_eq!(divergence_snippet("abc", "abc", 40), None);
    }

    #[test]
    fn divergence_snippet_grabs_window_around_first_diff() {
        let original = "prefix here. P, P, P, P, P, P, P, P, done";
        let cleaned = "prefix here. P, done";
        let snippet = divergence_snippet(original, cleaned, 20).unwrap();
        assert!(snippet.contains("P,"), "snippet missing P,: {snippet}");
        assert!(snippet.len() <= original.len());
    }

    #[test]
    fn divergence_snippet_handles_utf8() {
        // No panics on multi-byte codepoints either side of the diverge index.
        let original = "café P P P P P P P end";
        let cleaned = "café P end";
        let snippet = divergence_snippet(original, cleaned, 10).unwrap();
        assert!(snippet.contains('P'));
    }

    #[test]
    fn qc_verdict_zero_pages_treated_as_one() {
        // Defensive: page count of 0 shouldn't divide-by-zero or let
        // runaway truncations through.
        assert_eq!(qc_verdict(21, 0, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn longest_run_catches_phrase_loop() {
        // F1 incident shape: 600 reps of "the retrieval of " is ~10 KB
        // of contiguous run. Word-3-gram detection at LOOP_MIN_REPS=4
        // catches it.
        let input = "the retrieval of ".repeat(600);
        let run = longest_repeated_run_bytes(&input);
        assert!(
            run >= 1024,
            "expected ≥1024 byte run, got {run} (input is {} bytes)",
            input.len()
        );
    }

    #[test]
    fn longest_run_zero_for_clean_prose() {
        let input = "Retrieval-augmented generation combines pretrained \
                     parametric memory with non-parametric retrieval to \
                     improve factual accuracy on knowledge-intensive tasks.";
        assert_eq!(longest_repeated_run_bytes(input), 0);
    }

    #[test]
    fn longest_run_below_floor_ignored() {
        // Three consecutive repeats is under LOOP_MIN_REPS=4 — natural
        // prose ("and the cat and the dog and the bird") shouldn't trip.
        let input = "and the cat and the dog and the bird";
        assert_eq!(longest_repeated_run_bytes(input), 0);
    }

    #[test]
    fn longest_run_catches_char_loop() {
        // "ggggggg..." style char-level loop, 50 chars wide.
        let input = format!("prefix {} suffix", "g".repeat(50));
        let run = longest_repeated_run_bytes(&input);
        assert!(run >= 50, "expected ≥50, got {run}");
    }

    #[test]
    fn longest_run_handles_utf8() {
        // No panics on multi-byte separators around the loop.
        let input = format!("café {} café", "the model ".repeat(20));
        let run = longest_repeated_run_bytes(&input);
        assert!(run > 0, "should detect the loop");
    }

    #[test]
    fn unigram_under_threshold_preserved() {
        // Four consecutive repeats of a unigram is under threshold — keep them.
        let input = "A A A A rest";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count, 0);
    }
}
