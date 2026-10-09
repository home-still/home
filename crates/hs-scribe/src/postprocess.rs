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
    /// The server left regions out of the markdown because it could not
    /// process them, so the document has holes. Caller must not record it
    /// as a successful conversion; a different backend may do better.
    RejectGapped,
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

/// Longest pre-cleanup repeated run, in bytes, below which a document has no
/// runaway loop at all. A real VLM loop spams a phrase toward max_tokens,
/// leaving a run far longer than any legitimate repeat; below this floor the
/// truncation sites are benign short-form repetition (code indentation, table
/// rules, symbol runs). Gating the count-based reject below this floor stops
/// long technical books — hundreds of ~1-line truncations whose longest run is
/// a few dozen bytes — from being rejected on truncation COUNT, which scales
/// with document length rather than loopiness.
const QC_LOOP_RUN_FLOOR: usize = 128;
/// Per-page truncation budget that makes the absolute ceiling length-aware:
/// the effective ceiling is `max(QC_ABSOLUTE_MAX, pages * this)`, so a flat cap
/// no longer punishes long documents. A whole-doc average above this (with a
/// real repeated run present) is the distributed-loop signal.
const QC_TRUNC_PER_PAGE_BUDGET: usize = 2;
/// Minimum truncation sites on a page for it to count as "bad" in the spread
/// gate. A lone scattered site per page is normal in dense technical text and
/// no longer flags the page.
const QC_BAD_PAGE_MIN_TRUNCS: usize = 2;

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
/// is trustworthy. The definitive runaway-loop signal is `longest_run_bytes`
/// (computed on the **original** pre-cleanup markdown): a real VLM loop spams a
/// phrase toward max_tokens, leaving a run far longer than any legitimate
/// repeat. Below `QC_LOOP_RUN_FLOOR` there is no loop, so the document is
/// accepted regardless of truncation count — this is what stops long technical
/// books from being rejected on COUNT, which scales with length not loopiness.
/// Once a meaningful run exists, it trips on any of:
/// - total truncation count > `max(QC_ABSOLUTE_MAX, pages × QC_TRUNC_PER_PAGE_BUDGET)`
/// - any single page with `truncations > QC_PER_PAGE_MAX` (or × the
///   bibliography multiplier when that page's region classes flag it)
/// - more than `QC_BAD_PAGE_RATIO_PCT`% of pages with `>= QC_BAD_PAGE_MIN_TRUNCS`
///   truncation sites (a lone scattered site per page is normal)
/// - longest contiguous repeated-substring run > `QC_LONGEST_RUN_BYTES_MAX`
///
/// Independently of all of that, any `skipped_regions` (regions the pipeline
/// could not process and dropped from the markdown) is `RejectGapped`: a
/// loop-free document with a hole in it is still not a conversion.
///
/// `per_page_truncations` and `per_page_is_bibliography` must have the same
/// length and index alignment.
pub fn qc_verdict(
    per_page_truncations: &[crate::diag::TruncationCounts],
    per_page_is_bibliography: &[bool],
    longest_run_bytes: usize,
    skipped_regions: usize,
) -> QcVerdict {
    debug_assert_eq!(
        per_page_truncations.len(),
        per_page_is_bibliography.len(),
        "qc_verdict: per_page vec lengths must match"
    );

    if skipped_regions > 0 {
        return QcVerdict::RejectGapped;
    }
    // No meaningful repeated run anywhere ⇒ no runaway loop. The truncation
    // sites are benign short-form repetition (code indentation, table rules);
    // accept regardless of their count, which otherwise grows with document
    // length and false-positives long code-dense books.
    if longest_run_bytes < QC_LOOP_RUN_FLOOR {
        return QcVerdict::Accept;
    }

    let total_pages = per_page_truncations.len().max(1);

    // Absolute ceiling, made length-aware: a flat cap punishes long documents,
    // so scale the budget with page count and keep QC_ABSOLUTE_MAX as a floor.
    let total: usize = per_page_truncations.iter().map(|t| t.total()).sum();
    let absolute_ceiling =
        QC_ABSOLUTE_MAX.max(total_pages.saturating_mul(QC_TRUNC_PER_PAGE_BUDGET));
    if total > absolute_ceiling {
        return QcVerdict::RejectLoop;
    }

    for (i, t) in per_page_truncations.iter().enumerate() {
        let is_bib = per_page_is_bibliography.get(i).copied().unwrap_or(false);
        let ceiling = if is_bib {
            QC_PER_PAGE_MAX.saturating_mul(QC_BIBLIOGRAPHY_MULTIPLIER)
        } else {
            QC_PER_PAGE_MAX
        };
        if t.total() > ceiling {
            return QcVerdict::RejectLoop;
        }
    }

    // Spread gate: many pages each carrying real (>= QC_BAD_PAGE_MIN_TRUNCS)
    // truncation activity. A lone scattered site per page is normal in dense
    // technical text and no longer counts as a "bad" page.
    let bad_pages = per_page_truncations
        .iter()
        .filter(|t| t.total() >= QC_BAD_PAGE_MIN_TRUNCS)
        .count();
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
    // Word-n-gram level: process line by line — `clean_ngram_repetitions`
    // does the same. n=3 catches the F1 incident ("the retrieval of "
    // repeated); the 1..=4 sweep covers the rest of the cleaning passes.
    let mut max_run = longest_char_run(text);
    for line in text.split('\n') {
        for n in 1..=4 {
            max_run = max_run.max(longest_ngram_run(line, n));
        }
    }
    max_run
}

/// Character-level: walk the whole text. Char runs aren't constrained to
/// single lines (a `gggggg...` run can span newlines). One pass, no
/// per-character storage.
fn longest_char_run(text: &str) -> usize {
    let mut max_run = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let mut reps = 1;
        let mut end = start + ch.len_utf8();
        while let Some(&(at, next)) = chars.peek() {
            if next != ch {
                break;
            }
            reps += 1;
            end = at + next.len_utf8();
            chars.next();
        }
        if reps >= LOOP_MIN_REPS {
            max_run = max_run.max(end - start);
        }
    }
    max_run
}

/// Byte length of the longest span of `line` that is one `n`-word block
/// repeated at least `LOOP_MIN_REPS` times back to back (whole blocks only;
/// words are whitespace-separated).
fn longest_ngram_run(line: &str, n: usize) -> usize {
    debug_assert!((1..=MAX_NGRAM).contains(&n));
    let mut words = WordWindow::new(line);
    let mut max_run = 0;
    let mut i = 0;
    while let Some((start_byte, _)) = words.get(i) {
        // The block at `i` must be complete: a partial block cannot repeat.
        let mut pattern = [""; MAX_NGRAM];
        let mut end_byte = 0;
        for (k, slot) in pattern[..n].iter_mut().enumerate() {
            let Some((at, word)) = words.get(i + k) else {
                return max_run;
            };
            *slot = word;
            end_byte = at + word.len();
        }
        let mut reps = 1;
        let mut j = i + n;
        loop {
            let mut block_end = 0;
            let repeats = pattern[..n]
                .iter()
                .enumerate()
                .all(|(k, want)| match words.get(j + k) {
                    Some((at, got)) if got == *want => {
                        block_end = at + got.len();
                        true
                    }
                    _ => false,
                });
            if !repeats {
                break;
            }
            reps += 1;
            end_byte = block_end;
            j += n;
            if reps >= LOOP_MIN_REPS {
                // The run is counted whole and the scan resumes after it, so
                // nothing before `j` is read again.
                words.drop_before(j);
            }
        }
        if reps >= LOOP_MIN_REPS {
            max_run = max_run.max(end_byte - start_byte);
            i = j;
        } else {
            i += 1;
        }
        words.drop_before(i);
    }
    max_run
}

/// Longest word n-gram the loop cleaners and the QC scan consider.
const MAX_NGRAM: usize = 4;

/// A lazily tokenized line (`(byte_offset, word)` pairs, whitespace via
/// `char::is_whitespace` so multi-byte separators don't land inside a word)
/// that keeps only the words between the scan position and its lookahead.
/// Memory is bounded by `(LOOP_MIN_REPS + 1) * MAX_NGRAM` words, not by the
/// line (a markdown document with no newline is one line).
struct WordWindow<'a> {
    line: &'a str,
    rest: std::str::SplitWhitespace<'a>,
    buf: std::collections::VecDeque<(usize, &'a str)>,
    /// Absolute index of `buf[0]`.
    base: usize,
}

impl<'a> WordWindow<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            line,
            rest: line.split_whitespace(),
            buf: std::collections::VecDeque::new(),
            base: 0,
        }
    }

    /// The `k`-th word of the line (`k` must not precede the window).
    fn get(&mut self, k: usize) -> Option<(usize, &'a str)> {
        debug_assert!(k >= self.base);
        while self.base + self.buf.len() <= k {
            let word = self.rest.next()?;
            let offset = word.as_ptr() as usize - self.line.as_ptr() as usize;
            self.buf.push_back((offset, word));
        }
        Some(self.buf[k - self.base])
    }

    /// Forget the words before `k`.
    fn drop_before(&mut self, k: usize) {
        let n = k.saturating_sub(self.base).min(self.buf.len());
        self.buf.drain(..n);
        self.base += n;
    }
}

/// Clean repetition artifacts from a doc-wide markdown string per-page,
/// returning the cleaned markdown plus a `Vec<TruncationCounts>` of
/// per-page breakdowns (index-aligned with `compute_page_offsets`).
///
/// Pages are split on the same `\n\n---\n\n` separator that
/// [`hs_common::catalog::compute_page_offsets`] uses, so the returned
/// per-page counts correspond 1:1 with the offset entries downstream.
///
/// Invariant: the sum of `.total()` across the returned vec equals
/// `clean_repetitions(text).1.total()` for the same input.
pub fn clean_repetitions_per_page(text: &str) -> (String, Vec<crate::diag::TruncationCounts>) {
    const SEPARATOR: &str = "\n\n---\n\n";
    let mut cleaned_pages: Vec<String> = Vec::new();
    let mut per_page_counts: Vec<crate::diag::TruncationCounts> = Vec::new();
    for page in text.split(SEPARATOR) {
        let (cleaned, count) = clean_repetitions(page);
        cleaned_pages.push(cleaned);
        per_page_counts.push(count);
    }
    (cleaned_pages.join(SEPARATOR), per_page_counts)
}

/// Clean repetition artifacts from markdown text.
///
/// Returns `(cleaned_text, breakdown)` where `breakdown` is the per-pass
/// truncation count. Total truncations = `breakdown.total()`.
pub fn clean_repetitions(text: &str) -> (String, crate::diag::TruncationCounts) {
    let mut result = text.to_string();
    let mut breakdown = crate::diag::TruncationCounts::default();

    // Pass 1: character-level repetition (>10 consecutive identical chars)
    let (cleaned, count) = clean_char_repetitions(&result);
    result = cleaned;
    breakdown.char += count;

    // Pass 2: word n-gram repetition (4-gram repeating >3 consecutive times)
    let (cleaned, count) = clean_ngram_repetitions(&result, 4, 3);
    result = cleaned;
    breakdown.ngram4 += count;

    // Pass 3: shorter n-grams (2-gram repeating >4 consecutive times)
    let (cleaned, count) = clean_ngram_repetitions(&result, 2, 4);
    result = cleaned;
    breakdown.ngram2 += count;

    // Pass 4: unigram runs (same word repeating >5 consecutive times).
    // Catches VLM artifacts like "P, P, P, P, P" and ". . . . ." where
    // alternating tokens prevent the 2-gram pass from engaging.
    let (cleaned, count) = clean_ngram_repetitions(&result, 1, 5);
    result = cleaned;
    breakdown.ngram1 += count;

    (result, breakdown)
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

        let truncations_before = truncations;
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

        // A line with nothing truncated keeps its own whitespace (code
        // indentation, nested list markers, table alignment).
        if truncations == truncations_before {
            result_lines.push(line.to_string());
        } else {
            result_lines.push(cleaned_words.join(" "));
        }
    }

    (result_lines.join("\n"), truncations)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::diag::TruncationCounts;

    /// Build a TruncationCounts with all the count parked in `.char`
    /// for tests that only care about totals (qc_verdict semantics).
    fn tc(n: usize) -> TruncationCounts {
        TruncationCounts {
            char: n,
            ..Default::default()
        }
    }

    #[test]
    fn char_repetition_truncated() {
        let input = "hello ggggggggggggggggggg world";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, "hello g world");
        assert_eq!(count.total(), 1);
        assert_eq!(count.char, 1);
    }

    #[test]
    fn ngram_repetition_truncated() {
        let input =
            "and modeling and modeling and modeling and modeling and modeling and modeling done";
        let (output, count) = clean_repetitions(input);
        assert!(output.contains("and modeling"));
        assert!(!output.contains("and modeling and modeling and modeling and modeling"));
        assert!(output.ends_with("done"));
        assert!(count.total() > 0);
        assert!(count.ngram2 > 0 || count.ngram4 > 0);
    }

    #[test]
    fn short_text_unchanged() {
        let input = "This is fine.";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count.total(), 0);
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
        assert!(count.total() > 0);
    }

    #[test]
    fn multiline_preserved() {
        let input = "Line one\n\nLine two\n\n---\n\nLine three";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count.total(), 0);
    }

    #[test]
    fn mixed_repetitions() {
        let input = "eseseseseseseseseseseseseses and the model the model the model the model the model the model end";
        let (output, count) = clean_repetitions(input);
        assert!(count.total() >= 1); // "the model" bigram repetition is caught
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
        assert!(count.total() > 0, "expected at least one truncation");
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

    fn pages_with(truncations: &[TruncationCounts]) -> Vec<bool> {
        // Default: no bibliography pages.
        vec![false; truncations.len()]
    }

    #[test]
    fn qc_verdict_accepts_clean_doc() {
        let truncs = vec![tc(0); 10];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0, 0), QcVerdict::Accept);
        // 1 truncation on a single page in a 100-page doc — within the
        // 10% bad-page ratio gate.
        let mut truncs = vec![tc(0); 100];
        truncs[0] = tc(1);
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_absolute_runaway() {
        // Doc-wide total > 20 = reject. Spread across multiple pages so
        // no single page trips the per-page gate first.
        let truncs = vec![tc(3); 7]; // total = 21 > max(20, 7*2)
        let bib = pages_with(&truncs);
        // A real run is present (>= floor) AND the total is over the
        // length-aware ceiling.
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_per_page_runaway() {
        // Single page > 3 truncations → reject (one bad page poisons doc).
        let truncs = vec![tc(0), tc(4), tc(0), tc(0)]; // page 1 has 4 > 3
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_clean_long_docs() {
        // 30-page survey with 0 truncations on every page.
        let truncs = vec![tc(0); 30];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 0, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_rejects_single_long_run() {
        // F1 incident shape: one 9.4 KB contiguous run of "the retrieval of"
        // collapses to a single truncation site on one page. Per-page = 1
        // (passes) but longest-run gate trips.
        let truncs = vec![tc(1), tc(0), tc(0)];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 9400, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_short_legitimate_repeats() {
        // Citation boilerplate / table separators stay below 1 KB.
        let truncs = vec![tc(0); 10];
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 800, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_bibliography_page_gets_3x_ceiling() {
        // 8 truncations on a bibliography page passes (≤9), but trips
        // the 10% bad-page ratio (1/4 = 25% > 10%) — so use a longer doc.
        let truncs = vec![
            tc(8),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
            tc(0),
        ]; // 1/10 = 10%, not > 10%
        let bib = vec![
            true, false, false, false, false, false, false, false, false, false,
        ];
        // 8 ≤ 3*3 = 9, so per-page gate passes; bad-page ratio is 10% (not > 10%).
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_bibliography_page_still_caps_at_multiplier() {
        // A bibliography page with > 9 truncations is still rejected.
        let truncs = vec![tc(10), tc(0), tc(0), tc(0)];
        let bib = vec![true, false, false, false];
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_too_many_loopy_pages() {
        // 11% of pages each carrying real (>= 2) truncation activity → reject
        // even when no single page exceeds the per-page ceiling. A meaningful
        // run is present so the count gates are live.
        let mut truncs = vec![tc(0); 100];
        for t in truncs.iter_mut().take(11) {
            *t = tc(2);
        }
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_tolerates_at_threshold_loopy_pages() {
        // 10% exactly → accept (10 of 100 pages with >= 2 truncations).
        let mut truncs = vec![tc(0); 100];
        for t in truncs.iter_mut().take(10) {
            *t = tc(2);
        }
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 200, 0), QcVerdict::Accept);
    }

    #[test]
    fn qc_verdict_accepts_long_code_book_with_tiny_runs() {
        // feathers regression: a ~456-page code book yields hundreds of tiny,
        // scattered truncations (benign code/table formatting), but the longest
        // repeated run is only a few dozen bytes — there is no loop. The old
        // absolute-count gate rejected this purely for being long; it must now
        // be accepted because the longest run is below the loop floor.
        let truncs = vec![tc(2); 456]; // 912 truncation sites
        let bib = pages_with(&truncs);
        assert_eq!(qc_verdict(&truncs, &bib, 48, 0), QcVerdict::Accept);
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
        let per_page_sum: usize = per_page.iter().map(|t| t.total()).sum();
        assert_eq!(per_page.len(), 4);
        assert_eq!(per_page_sum, doc_total.total());
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
        let truncs: Vec<TruncationCounts> = vec![];
        let bib: Vec<bool> = vec![];
        assert_eq!(qc_verdict(&truncs, &bib, 0, 0), QcVerdict::Accept);
        assert_eq!(qc_verdict(&truncs, &bib, 9999, 0), QcVerdict::RejectLoop);
    }

    #[test]
    fn qc_verdict_rejects_a_document_with_holes_even_when_it_has_no_loops() {
        // Skipped regions are lost content, whatever else is clean: a clean
        // loop-free doc, a doc with no pages at all, and a bibliography-heavy
        // doc are all refused.
        let truncs = vec![crate::diag::TruncationCounts::default(); 5];
        let bib = vec![false; 5];
        assert_eq!(qc_verdict(&truncs, &bib, 0, 0), QcVerdict::Accept);
        assert_eq!(qc_verdict(&truncs, &bib, 0, 1), QcVerdict::RejectGapped);
        assert_eq!(qc_verdict(&[], &[], 0, 3), QcVerdict::RejectGapped);
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
        assert_eq!(count.total(), 0);
    }

    #[test]
    fn lines_without_truncation_keep_their_whitespace() {
        let input = "    indented code line here\n  - nested  item two";
        let (output, count) = clean_repetitions(input);
        assert_eq!(output, input);
        assert_eq!(count.total(), 0);
    }

    /// The previous implementation, kept as the oracle: it materialises a
    /// `Vec<(usize, char)>` for the whole text (16 B/char) and a word vector
    /// per line, which is what the streaming scan replaced.
    fn oracle_longest_repeated_run_bytes(text: &str) -> usize {
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

        let mut max_run = 0;
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut i = 0;
        while i < chars.len() {
            let (start_byte, ch) = chars[i];
            let mut j = i + 1;
            while j < chars.len() && chars[j].1 == ch {
                j += 1;
            }
            if j - i >= LOOP_MIN_REPS {
                let end_byte = chars.get(j).map(|(b, _)| *b).unwrap_or(text.len());
                max_run = max_run.max(end_byte - start_byte);
            }
            i = j;
        }
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
                        max_run = max_run.max(last_start + last_word.len() - start_byte);
                        i = j;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        max_run
    }

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn the_streaming_scan_matches_the_oracle_on_random_and_adversarial_text() {
        const WORDS: [&str; 9] = ["a", "b", "ab", "ba", "the", "é", "日本", "x\u{301}", "zz"];
        const SEPS: [&str; 7] = [" ", " ", "  ", "\n", "\t", "\u{3000}", "\u{a0}\n"];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut cases: Vec<String> = Vec::new();

        // Random word soup over a tiny alphabet so runs of every n occur.
        for _ in 0..4000 {
            let alphabet = 1 + rng.below(WORDS.len());
            let len = rng.below(60);
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(WORDS[rng.below(alphabet)]);
                s.push_str(SEPS[rng.below(SEPS.len())]);
            }
            cases.push(s);
        }
        // Periodic blocks (every n, every repeat count around the threshold)
        // with a noise prefix, a perturbed middle repeat and a tail.
        for n in 1..=5usize {
            for reps in 1..=9usize {
                for noise in 0..4usize {
                    let block: Vec<&str> =
                        (0..n).map(|k| WORDS[(k + noise) % WORDS.len()]).collect();
                    let mut s = "p q ".repeat(noise);
                    for r in 0..reps {
                        for (k, w) in block.iter().enumerate() {
                            if r == reps / 2 && k == n - 1 && noise % 2 == 1 {
                                s.push_str("DIFFERENT ");
                            } else {
                                s.push_str(w);
                                s.push(' ');
                            }
                        }
                    }
                    s.push_str("tail\nnext line a a a a a a");
                    cases.push(s);
                }
            }
        }
        // Character runs: lengths around the threshold, multi-byte, across
        // newlines, adjacent runs of different characters.
        for len in 0..9usize {
            for ch in ['g', 'é', '日', '\n', ' ', '😀'] {
                let run: String = std::iter::repeat_n(ch, len).collect();
                cases.push(format!("x{run}y{run}{run}z"));
                cases.push(format!("{run}{run}"));
            }
        }
        // Whole-text edge cases and one very long run.
        for s in [
            "",
            " ",
            "\n",
            "\n\n\n",
            "a",
            "a a a a",
            "a a a a\n",
            " a a a a ",
        ] {
            cases.push(s.to_string());
        }
        cases.push("the retrieval of ".repeat(600));
        cases.push(format!("{}tail", "a b a c ".repeat(500)));
        cases.push(format!("{} z z z z z z", "q ".repeat(3)));

        for text in &cases {
            assert_eq!(
                longest_repeated_run_bytes(text),
                oracle_longest_repeated_run_bytes(text),
                "diverged on {text:?}"
            );
        }
    }

    #[test]
    fn a_long_run_does_not_buffer_the_line() {
        // 4 M words of one 3-gram on a single line: the window holds a few
        // words, so this is bounded memory (and finishes quickly).
        let text = "the retrieval of ".repeat(1_400_000);
        assert_eq!(longest_repeated_run_bytes(&text), text.len() - 1);
    }
}
