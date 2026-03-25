use super::cdm::cdm_score_multi;
use super::edit_distance::{omnidocbench_text_score, omnidocbench_text_score_blocks};
use super::teds::teds_score;

/// Result of evaluating a single page/sample.
#[derive(Debug, Clone)]
pub struct CompositeScore {
    /// None when the reference has no text annotations (text not applicable for this page).
    pub text_score: Option<f64>,
    pub teds_score: Option<f64>,
    pub cdm_score: Option<f64>,
    pub composite: f64,
}

/// Compute OmniDocBench composite: ((1-ED)*100 + TEDS + CDM) / N
/// Only counts metrics that are actually available.
/// When reference has no text annotations, text_score is None (not scored).
/// When reference text is empty/trivial but has_text_ref is true, excludes
/// text from composite (only uses structural metrics like TEDS/CDM if available).
/// Uses official per-block Hungarian matching with length-weighted NED
/// (sum(ED)/sum(max_len)) when blocks are available.
pub fn omnidocbench_composite(
    reference_text: &str,
    hypothesis_text: &str,
    reference_blocks: Option<&[String]>,
    hypothesis_blocks: Option<&[String]>,
    reference_table_html: Option<&str>,
    hypothesis_table_html: Option<&str>,
    reference_formula_latex: Option<&[String]>,
    hypothesis_formula_latex: Option<&[String]>,
    has_text_ref: bool,
) -> CompositeScore {
    // Official v1.5: if reference has tables/formulas, score must be computed.
    // Missing hypothesis = score 0 (not excluded).
    let teds = match (reference_table_html, hypothesis_table_html) {
        (Some(r), Some(h)) => teds_score(r, h),
        (Some(_), None) => Some(0.0), // ref has table, we extracted nothing
        _ => None, // no ref table = not scored
    };

    let cdm = match (reference_formula_latex, hypothesis_formula_latex) {
        (Some(r), Some(h)) => cdm_score_multi(r, h),
        (Some(_), None) => Some(0.0), // ref has formula, we extracted nothing
        _ => None, // no ref formula = not scored
    };

    // Text score: None when reference has no text annotations at all.
    // This matches official scoring where each metric only applies to pages
    // that have the corresponding annotation type.
    let text_score = if has_text_ref {
        let text = match (reference_blocks, hypothesis_blocks) {
            (Some(ref_b), Some(hyp_b)) if !ref_b.is_empty() && !hyp_b.is_empty() => {
                omnidocbench_text_score_blocks(ref_b, hyp_b)
            }
            _ => omnidocbench_text_score(reference_text, hypothesis_text),
        };
        Some(text)
    } else {
        None
    };

    // When reference text is empty/trivial, don't penalize text_score
    // (the page may be table-only or formula-only with incomplete annotations)
    let ref_trimmed = reference_text.trim();
    let has_structural = teds.is_some() || cdm.is_some();
    let include_text = text_score.is_some() && (ref_trimmed.len() >= 5 || !has_structural);

    let mut sum = 0.0;
    let mut count = 0;

    if include_text {
        sum += text_score.unwrap();
        count += 1;
    }

    if let Some(t) = teds {
        sum += t;
        count += 1;
    }
    if let Some(c) = cdm {
        sum += c;
        count += 1;
    }

    // Fallback: if nothing scored, use text_score (or 0 if no text ref)
    if count == 0 {
        sum = text_score.unwrap_or(0.0);
        count = 1;
    }

    CompositeScore {
        text_score,
        teds_score: teds,
        cdm_score: cdm,
        composite: sum / count as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_only_composite() {
        let score = omnidocbench_composite("hello world", "hello world", None, None, None, None, None, None, true);
        assert_eq!(score.text_score, Some(100.0));
        assert_eq!(score.composite, 100.0);
        assert!(score.teds_score.is_none());
        assert!(score.cdm_score.is_none());
    }

    #[test]
    fn test_partial_text_composite() {
        let score = omnidocbench_composite("abc", "xyz", None, None, None, None, None, None, true);
        assert_eq!(score.text_score, Some(0.0));
        assert_eq!(score.composite, 0.0);
    }

    #[test]
    fn test_no_text_ref_composite() {
        // Page with no text annotations — text_score should be None
        let score = omnidocbench_composite("", "lots of text here", None, None, None, None, None, None, false);
        assert_eq!(score.text_score, None);
        // With no metrics at all, composite falls back to 0
        assert_eq!(score.composite, 0.0);
    }

    #[test]
    fn test_no_text_ref_with_table() {
        // Table-only page — text excluded, composite = TEDS only
        let score = omnidocbench_composite(
            "", "extracted text",
            None, None,
            Some("<table><tr><td>a</td></tr></table>"),
            Some("<table><tr><td>a</td></tr></table>"),
            None, None, false,
        );
        assert_eq!(score.text_score, None);
        assert!(score.teds_score.is_some());
    }

    #[test]
    fn test_block_text_scoring() {
        // Per-block scoring with matching blocks
        let ref_blocks = vec!["hello world".to_string(), "foo bar".to_string()];
        let hyp_blocks = vec!["hello world".to_string(), "foo bar".to_string()];
        let score = omnidocbench_composite(
            "hello world\nfoo bar", "hello world\nfoo bar",
            Some(&ref_blocks), Some(&hyp_blocks),
            None, None, None, None, true,
        );
        assert_eq!(score.text_score, Some(100.0));
    }

    #[test]
    fn test_fallback_to_concatenated() {
        // No blocks → falls back to concatenated
        let score = omnidocbench_composite(
            "hello world foo bar", "hello world foo bar",
            None, None,
            None, None, None, None, true,
        );
        assert_eq!(score.text_score, Some(100.0));
    }
}
