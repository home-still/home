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
/// Per-page truncation ceiling for non-bibliography pages. A single page
/// with more than this many truncation sites is loopy enough to reject
/// the whole document — long-tail dilution of the absolute ceiling no
/// longer hides a single bad page.
const QC_PER_PAGE_MAX: usize = 3;
/// Bibliography pages legitimately repeat citation boilerplate ("et al.",
/// year prefixes, separator chars), so we apply a 3× multiplier — effective
/// per-page ceiling = 9 — to avoid over-truncating clean reference lists.
/// Non-bibliography pages stay strict.
const QC_BIBLIOGRAPHY_MULTIPLIER: usize = 3;
/// Maximum percentage of pages allowed to have any truncation activity.
/// Catches the "many slightly-loopy pages" mode that no per-page or
/// absolute gate trips: 100 pages × 1 truncation each evades both, but
/// 100% of pages being touched is itself the failure signal.
const QC_BAD_PAGE_RATIO_PCT: usize = 10;
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

/// PP-DocLayout-V3 region class names that indicate a bibliography page.
/// These are the canonical strings emitted by `models/layout.rs` (idx 18,
/// 19 in the 25-class taxonomy). A page with any region of these classes
/// gets the `QC_BIBLIOGRAPHY_MULTIPLIER` applied to its per-page ceiling.
const BIBLIOGRAPHY_CLASSES: &[&str] = &["reference", "reference_content"];

/// Returns true if any of the page's PP-DocLayout-V3 region classes marks
/// the page as bibliography content. Empty class lists (e.g. blank pages,
/// FullPage-mode pages with no layout info) are NOT bibliography — they
/// get the strict default ceiling, which is the safer choice.
pub fn is_bibliography_page(class_names: &[String]) -> bool {
    class_names
        .iter()
        .any(|c| BIBLIOGRAPHY_CLASSES.contains(&c.as_str()))
}

/// Decide whether the markdown that came out of `clean_repetitions_per_page`
/// is trustworthy. Trips on any of:
/// - total truncation count > `QC_ABSOLUTE_MAX`
/// - any single page with `truncations > QC_PER_PAGE_MAX` (or × the
///   bibliography multiplier when that page's region classes flag it)
/// - more than `QC_BAD_PAGE_RATIO_PCT`% of pages have any truncation activity
/// - longest contiguous repeated-substring run > `QC_LONGEST_RUN_BYTES_MAX`
///
/// `per_page_truncations` and `per_page_is_bibliography` must have the same
/// length and index alignment. `longest_run_bytes` is computed on the
/// **original** (pre-cleanup) markdown via [`longest_repeated_run_bytes`].
pub fn qc_verdict(
    per_page_truncations: &[usize],
    per_page_is_bibliography: &[bool],
    longest_run_bytes: usize,
) -> QcVerdict {
    debug_assert_eq!(
        per_page_truncations.len(),
        per_page_is_bibliography.len(),
        "qc_verdict: per_page vec lengths must match"
    );

    let total: usize = per_page_truncations.iter().sum();
    if total > QC_ABSOLUTE_MAX {
        return QcVerdict::RejectLoop;
    }

    for (i, &t) in per_page_truncations.iter().enumerate() {
        let is_bib = per_page_is_bibliography.get(i).copied().unwrap_or(false);
        let ceiling = if is_bib {
            QC_PER_PAGE_MAX.saturating_mul(QC_BIBLIOGRAPHY_MULTIPLIER)
        } else {
            QC_PER_PAGE_MAX
        };
        if t > ceiling {
            return QcVerdict::RejectLoop;
        }
    }

    let total_pages = per_page_truncations.len().max(1);
    let bad_pages = per_page_truncations.iter().filter(|&&t| t >= 1).count();
    if bad_pages.saturating_mul(100) > total_pages.saturating_mul(QC_BAD_PAGE_RATIO_PCT) {
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

/// Clean repetition artifacts from a doc-wide markdown string per-page,
/// returning the cleaned markdown plus a `Vec<usize>` of per-page
/// truncation counts (index-aligned with `compute_page_offsets`).
///
/// Pages are split on the same `\n\n---\n\n` separator that
/// [`hs_common::catalog::compute_page_offsets`] uses, so the returned
/// per-page counts correspond 1:1 with the offset entries downstream.
///
/// Invariant: the sum of returned counts equals
/// `clean_repetitions(text).1` for the same input.
pub fn clean_repetitions_per_page(text: &str) -> (String, Vec<usize>) {
    const SEPARATOR: &str = "\n\n---\n\n";
    let mut cleaned_pages: Vec<String> = Vec::new();
    let mut per_page_counts: Vec<usize> = Vec::new();
    for page in text.split(SEPARATOR) {
        let (cleaned, count) = clean_repetitions(page);
        cleaned_pages.push(cleaned);
        per_page_counts.push(count);
    }
    (cleaned_pages.join(SEPARATOR), per_page_counts)
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

    fn pages_with(truncations: &[usize]) -> Vec<bool> {
        // Default: no bibliography pages.
        vec![false; truncations.len()]
    }

    #[test]
    fn qc_verdict_accepts_clean_doc() {
        let truncs = vec![0; 10];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
        // 1 truncation on a single page in a 100-page doc — within the
        // 10% bad-page ratio gate.
        let mut truncs = vec![0; 100];
        truncs[0] = 1;
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_absolute_runaway() {
        // Doc-wide total > 20 = reject. Spread across multiple pages so
        // no single page trips the per-page gate first.
        let truncs = vec![3; 7]; // total = 21
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_per_page_runaway() {
        // Single page > 3 truncations → reject (one bad page poisons doc).
        let truncs = vec![0, 4, 0, 0]; // page 1 has 4 > 3
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_clean_long_docs() {
        // 30-page survey with 0 truncations on every page.
        let truncs = vec![0; 30];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_single_long_run() {
        // F1 incident shape: one 9.4 KB contiguous run of "the retrieval of"
        // collapses to a single truncation site on one page. Per-page = 1
        // (passes) but longest-run gate trips.
        let truncs = vec![1, 0, 0];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 9400), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_short_legitimate_repeats() {
        // Citation boilerplate / table separators stay below 1 KB.
        let truncs = vec![0; 10];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 800), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_bibliography_page_gets_3x_ceiling() {
        // 8 truncations on a bibliography page passes (≤9), but trips
        // the 10% bad-page ratio (1/4 = 25% > 10%) — so use a longer doc.
        let truncs = vec![8, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // 1/10 = 10%, not > 10%
        let bib = vec![
            true, false, false, false, false, false, false, false, false, false,
        ];
        // 8 ≤ 3*3 = 9, so per-page gate passes; bad-page ratio is 10% (not > 10%).
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_bibliography_page_still_caps_at_multiplier() {
        // A bibliography page with > 9 truncations is still rejected.
        let truncs = vec![10, 0, 0, 0];
        let bib = vec![true, false, false, false];
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_too_many_loopy_pages() {
        // 11% of pages with truncation activity → reject even when no
        // single page exceeds the ceiling and the absolute is fine.
        // 100 pages × (11 with 1 truncation, 89 with 0) = 11 total, 11%.
        let mut truncs = vec![0; 100];
        for t in truncs.iter_mut().take(11) {
            *t = 1;
        }
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_at_threshold_loopy_pages() {
        // 10% exactly → accept (10 of 100 pages with 1 truncation).
        let mut truncs = vec![0; 100];
        for t in truncs.iter_mut().take(10) {
            *t = 1;
        }
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
    }

    #[test]
    fn is_bibliography_page_detects_canonical_classes() {
        assert!(is_bibliography_page(&["reference".to_string()]));
        assert!(is_bibliography_page(&["reference_content".to_string()]));
        assert!(is_bibliography_page(&[
            "text".to_string(),
            "reference".to_string()
        ]));
    }

    #[test]
    fn is_bibliography_page_rejects_other_classes() {
        assert!(!is_bibliography_page(&[]));
        assert!(!is_bibliography_page(&["text".to_string()]));
        assert!(!is_bibliography_page(&[
            "abstract".to_string(),
            "paragraph_title".to_string()
        ]));
        // Substring matches must not trigger.
        assert!(!is_bibliography_page(&["xreference".to_string()]));
    }

    #[test]
    fn clean_repetitions_per_page_invariant_sum() {
        // Sum of per-page counts must equal the doc-wide count.
        let pages = [
            "clean text".to_string(),
            "P, P, P, P, P, P repeat".to_string(),
            "the retrieval of ".repeat(50),
            "more clean text".to_string(),
        ];
        let joined = pages.join("\n\n---\n\n");
        let (_, doc_total) = clean_repetitions(&joined);
        let (_, per_page) = clean_repetitions_per_page(&joined);
        let per_page_sum: usize = per_page.iter().sum();
        assert_eq!(per_page.len(), 4);
        assert_eq!(per_page_sum, doc_total);
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
        // Defensive: an empty per-page vec shouldn't divide-by-zero.
        // Doc-wide total = 0, no per-page entries to check, longest-run
        // dominant — used to assert 0-page input doesn't panic.
        let truncs: Vec<usize> = vec![];
        let bib: Vec<bool> = vec![];
        assert_eq!(qc_verdict(&truncs, &bib, 0), QcVerdict::Accept);
        assert_eq!(qc_verdict(&truncs, &bib, 9999), QcVerdict::RejectLoop);
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
