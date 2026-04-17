//! Post-processing for scribe markdown output.
//!
//! Detects and truncates repetition loops that the VLM model produces,
//! such as "and modeling and modeling and modeling..." or "ggggggg...".

/// Detect stub PDFs: 1-page results that are either short on content (landing
/// pages, paywalls) OR returned suspiciously fast (under a second — a real
/// VLM pass cannot complete in that time; the "content" is almost certainly a
/// cached or malformed response).
pub fn is_stub_pdf(total_pages: u64, markdown: &str, duration_secs: f64) -> bool {
    if total_pages > 1 {
        return false;
    }
    let non_ws = markdown.chars().filter(|c| !c.is_whitespace()).count();
    non_ws < 500 || duration_secs < 1.0
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
    fn stub_short_content() {
        // 1 page, few chars, plausible duration → stub
        let md = "# Title\n\nnot much here";
        assert!(is_stub_pdf(1, md, 2.5));
    }

    #[test]
    fn stub_sub_second() {
        // 1 page, plenty of chars, but impossibly fast → stub.
        // A real VLM pass cannot complete a page in under 1s; this
        // case catches cached/empty scribe responses that slipped
        // the char-count gate.
        let md = "x".repeat(2000);
        assert!(is_stub_pdf(1, &md, 0.0));
    }

    #[test]
    fn real_short_paper_not_stub() {
        // Multi-page, even with brief content → not a stub.
        let md = "# Short paper\n\nabstract body.";
        assert!(!is_stub_pdf(2, md, 0.5));
    }

    #[test]
    fn real_long_paper_not_stub() {
        // 1 page with enough real content and plausible duration.
        let md = "a".repeat(2000);
        assert!(!is_stub_pdf(1, &md, 3.0));
    }
}
