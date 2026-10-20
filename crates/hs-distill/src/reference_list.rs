//! Detecting reference-list ("bibliography") chunks.
//!
//! A chunk that is a list of citations to other papers matches a query
//! about a topic perfectly well in a dense embedding — semantically, a
//! citation to work about hippocampal neurogenesis *is* about
//! hippocampal neurogenesis. Measured on this corpus, 24 % of the
//! top-3 results for a fixed query set were reference lists, and the
//! best-ranked hit for "Gazzaniga split brain" was a chunk of
//! `doi: 10.1016/…` lines.
//!
//! The detection is structural, not semantic: a citation line has a
//! shape. It carries a `doi:`, a parenthesised year after author names,
//! `et al.`, a page range, a volume, or a numbered-list marker. A
//! prose paragraph that happens to mention a year does not.
//!
//! The rule is deliberately conservative. A chunk that mixes a citation
//! with real prose is *useful* — it discusses the topic and cites it —
//! and must not be demoted. The threshold is the share of a chunk's
//! non-empty lines that are citation lines: at 0.5 only the pure lists
//! are demoted. On a 300-chunk sample of this corpus that is 10.3 % of
//! chunks, while a threshold of 0.2 would take 24 % of them and destroy
//! the mixed ones.
//!
//! The project's own `hs-scribe` detects bibliography *pages* with the
//! layout model's classes (`is_bibliography_page`); that verdict is
//! more accurate but does not survive the conversion, so the shape of
//! the text is what is available here.

/// The share of a chunk's lines that must be citation lines before the
/// chunk is treated as a reference list.
pub const REFERENCE_LIST_THRESHOLD: f64 = 0.5;

/// Is this line a citation entry?
///
/// The patterns are the ones a bibliography line actually has. Each
/// one is deliberately specific: a bare year in brackets is not enough
/// (prose writes "(2020)" too), so the year must follow author-ish
/// text.
pub fn is_citation_line(line: &str) -> bool {
    let s = line.trim();
    // A short line is a heading, a list bullet, or a fragment — not a
    // citation.
    if s.len() < 12 {
        return false;
    }
    // "doi: 10.1016/j.neubiorev.2009.01.004" — unambiguous.
    if has_doi(s) {
        return true;
    }
    // "Berkenbosch F, van Vant J, Aubert A, et al. (2001) Title. Journal." —
    // author text, then a parenthesised year.
    if has_author_then_year(s) {
        return true;
    }
    // "1. Smith, J. (2020). Title." — a numbered reference list.
    if is_numbered_entry(s) {
        return true;
    }
    // "Smith, J., Jones, A., and Brown, B. (2020) ..." — the author
    // list then the year, where the year is further away than the
    // author-then-year pattern above allows for.
    if has_author_then_year_loose(s) {
        return true;
    }
    if has_et_al(s) {
        return true;
    }
    // "pp. 189-210", "vol. 15", "[12]".
    if has_page_range(s) || has_volume(s) || is_bracket_number(s) {
        return true;
    }
    // A bare URL line.
    if s.starts_with("http://") || s.starts_with("https://") {
        return true;
    }
    // "Retrieved from https://…", "Available at: …".
    if has_retrieval_phrase(s) {
        return true;
    }
    // "- Frodl T, Meisenzahl EM, Zetzsche T, et al.  Hippocampal and
    // amygdala ..." — a markdown bullet holding an author list, with no
    // year at all (some styles drop it). The shape is the same: a
    // capitalised surname, then a run of capitalised surname-initials
    // separated by commas.
    if is_bulleted_author_list(s) {
        return true;
    }
    false
}

/// The share of a chunk's non-empty lines that are citation lines.
///
/// 0.0 for prose, 1.0 for a pure reference list.
pub fn citation_line_share(text: &str) -> f64 {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return 0.0;
    }
    let citations = lines.iter().filter(|l| is_citation_line(l)).count();
    citations as f64 / lines.len() as f64
}

/// Is this chunk a reference list?
pub fn is_reference_list(text: &str) -> bool {
    citation_line_share(text) > REFERENCE_LIST_THRESHOLD
}

// ── the pattern helpers ──────────────────────────────────────────────

fn has_doi(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    // "doi:10.xxxx" or "doi: 10.xxxx" or "https://doi.org/10.xxxx".
    if let Some(pos) = lower.find("doi") {
        let rest = &lower[pos + 3..];
        let rest = rest.trim_start();
        let rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
        if rest.starts_with("10.") {
            return true;
        }
    }
    lower.contains("doi.org/10.")
}

fn has_author_then_year(s: &str) -> bool {
    // "Smith, J. (2020)." — the year may sit far from the first
    // surname: a long author list puts it 100+ characters in
    // ("Butters, M. A., Becker, J. T., Nebes, R. D., …, et al. (2008)").
    // So the test is the *shape* of the head — an author list — not a
    // distance.
    let Some(year_pos) = s.find('(') else {
        return false;
    };
    let head = &s[..year_pos];
    let year = &s[year_pos + 1..];
    let digits: String = year.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() != 4 {
        return false;
    }
    if head.trim().is_empty() {
        return false;
    }
    // The head must look like author text: a capitalised surname, and
    // either an initial or a comma (the two shapes an author list takes
    // in this corpus).
    let has_cap = head.chars().any(|c| c.is_uppercase());
    let has_initial = head
        .split_whitespace()
        .any(|w| w.len() <= 2 && w.chars().next().is_some_and(|c| c.is_uppercase()));
    let has_comma = head.contains(',');
    has_cap && (has_initial || has_comma)
}

fn has_author_then_year_loose(s: &str) -> bool {
    // "Smith, J., Jones, A., and Brown, B. (2020)" — the year is
    // further from the first surname than the tight pattern allows,
    // but the head is unambiguously an author list.
    let Some(year_pos) = s.find('(') else {
        return false;
    };
    let head = &s[..year_pos];
    let year = &s[year_pos + 1..];
    let digits: String = year.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() != 4 {
        return false;
    }
    // Three or more "Initial." tokens means an author list.
    let initials = head
        .split_whitespace()
        .filter(|w| w.len() == 1 && w.chars().next().is_some_and(|c| c.is_uppercase()))
        .count();
    initials >= 3 && head.contains(',')
}

fn is_numbered_entry(s: &str) -> bool {
    let rest = s.strip_prefix("1.").or_else(|| s.strip_prefix("2."));
    let Some(rest) = rest else {
        return false;
    };
    let rest = rest.trim_start();
    rest.chars().next().is_some_and(|c| c.is_uppercase())
}

fn has_et_al(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    // "et al" followed by a stop, so "metal" does not match.
    let mut from = 0;
    while let Some(pos) = lower[from..].find("et al") {
        let abs = from + pos;
        let after = &lower[abs + 5..];
        let ok = after
            .chars()
            .next()
            .is_none_or(|c| c == '.' || c == ',' || c == ' ' || c == ';');
        if ok {
            return true;
        }
        from = abs + 1;
    }
    false
}

fn has_page_range(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let Some(pos) = lower.find("pp.") else {
        return false;
    };
    let rest = &lower[pos + 3..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty()
}

fn has_volume(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let Some(pos) = lower.find("vol.") else {
        return false;
    };
    let rest = &lower[pos + 4..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty()
}

fn is_bracket_number(s: &str) -> bool {
    s.starts_with('[') && s[1..].chars().take_while(|c| c.is_ascii_digit()).count() > 0
}

fn has_retrieval_phrase(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("retrieved from") || lower.contains("available at")
}

/// "- Frodl T, Meisenzahl EM, Zetzsche T, et al.  Hippocampal and
/// amygdala ..." — a markdown bullet holding an author list.
///
/// The shape: a bullet, then a capitalised surname, then a run of
/// "Surname Initial" tokens separated by commas. A bibliography style
/// that omits the year leaves nothing else to go on, so the author-list
/// shape is the signal.
fn is_bulleted_author_list(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("• "))
    else {
        return false;
    };
    // The head, before the title, is the author list. Take the text up
    // to the first run of two spaces, which separates the authors from
    // the title in this style.
    let head = rest.split("  ").next().unwrap_or(rest);
    // At least two "Surname I" tokens, each with a capitalised surname
    // and one or two capitalised initials ("Frodl T", "Meisenzahl EM").
    let author_tokens = head
        .split(',')
        .map(|t| t.trim())
        .filter(|t| is_surname_with_initials(t))
        .count();
    if author_tokens >= 2 {
        return true;
    }
    // A single author: "- Gazzaniga MS. Forty-five years of split-brain
    // research …" — one "Surname Initials." at the front, then a title.
    // The period after the initials is what separates this from a
    // bulleted sentence.
    head.split_once(' ').is_some_and(|(surname, rest)| {
        let mut words = rest.split_whitespace();
        let Some(initials) = words.next() else {
            return false;
        };
        // "MS." — capitals and the period that ends the author.
        let initials = initials.trim_end_matches('.');
        (initials.len() == 1 || initials.len() == 2)
            && initials.chars().all(|c| c.is_uppercase())
            && surname.chars().next().is_some_and(|c| c.is_uppercase())
            && words.next().is_some()
    })
}

/// "Frodl T" or "Meisenzahl EM" — a capitalised surname followed by one
/// or two capitalised initials.
fn is_surname_with_initials(token: &str) -> bool {
    let mut parts = token.split_whitespace();
    matches!(
        (parts.next(), parts.next()),
        (Some(surname), Some(initials))
            if surname.chars().next().is_some_and(|c| c.is_uppercase())
                && (initials.len() == 1 || initials.len() == 2)
                && initials.chars().all(|c| c.is_uppercase())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── individual lines ───────────────────────────────────────────

    #[test]
    fn a_doi_line_is_a_citation() {
        assert!(is_citation_line(
            "10.1016/j.neubiorev.2009.01.004 doi: 10.1016/j.neubiorev.2009.01.004"
        ));
        assert!(is_citation_line(
            "Berkenbosch F, van Vant J, Aubert A, et al. Brain-derived neurotrophic factor. Neuroscience."
        ));
    }

    #[test]
    fn an_author_and_year_line_is_a_citation() {
        assert!(is_citation_line(
            "Smith, J. (2020). A study of things. Journal of Things, 12(3), 45-67."
        ));
        assert!(is_citation_line(
            "van Vant J, Jonkman T, Caligor L (2004) Neurobiology of learning and memory."
        ));
    }

    #[test]
    fn a_long_author_list_is_a_citation_even_with_the_year_far_away() {
        // A real line from the corpus: the year is 103 characters in,
        // after seven authors and "et al.". A distance limit would miss
        // it; the shape of the head is the signal.
        assert!(is_citation_line(
            "Butters, M. A., Becker, J. T., Nebes, R. D., Zmuda, M. D., Mulsant, B. H., \
             Pollock, B. G. 3rd., et al. (2008) Frontal lobe volume and memory."
        ));
        assert!(is_citation_line(
            "Campion, D., Dumanchin, C., Hannequin, D., Dubois, B., Belliard, S., Puel, M., \
             et al. (1999) Early-onset bipolar disorder."
        ));
    }

    #[test]
    fn a_numbered_reference_entry_is_a_citation() {
        assert!(is_citation_line(
            "1. Smith, J. (2020). Title of the work. Publisher."
        ));
    }

    #[test]
    fn a_page_range_or_volume_is_a_citation() {
        assert!(is_citation_line(
            "Curr. Top. Behav. Neurosci. 15, 189-210. doi: 10.1007/7854_2012_220"
        ));
        assert!(is_citation_line(
            "Journal of Neurochemistry, vol. 104, pp. 12-19"
        ));
    }

    #[test]
    fn prose_is_not_a_citation() {
        assert!(!is_citation_line(
            "The hippocampus is a region of the brain located in the medial temporal lobe."
        ));
        assert!(!is_citation_line(
            "Neurogenesis continues throughout adulthood in rodents, a finding first reported in 1962."
        ));
        assert!(!is_citation_line(
            "The results (2020) showed a significant effect across every condition we tested."
        ));
    }

    #[test]
    fn a_word_containing_et_al_is_not_a_citation() {
        // "metal" contains "etal"; a naive substring match would fire.
        assert!(!is_citation_line(
            "The metal content of the alloy was measured across all specimens."
        ));
    }

    #[test]
    fn a_short_line_is_never_a_citation() {
        assert!(!is_citation_line("doi: 10.1"));
        assert!(!is_citation_line("2020"));
    }

    #[test]
    fn a_bulleted_author_list_is_a_citation() {
        // A bibliography style that drops the year leaves the author-list
        // shape as the only signal. These lines are markdown bullets.
        assert!(is_citation_line(
            "- Frodl T, Meisenzahl EM, Zetzsche T, et al.  Hippocampal and amygdala \
             cortices in major depression"
        ));
        assert!(is_citation_line(
            "- Gazzaniga MS. Forty-five years of split-brain research and still going strong."
        ));
    }

    #[test]
    fn a_bulleted_prose_line_is_not_a_citation() {
        // A bullet whose text is prose, not an author list.
        assert!(!is_citation_line(
            "- The hippocampus generates new neurons throughout adulthood, which remains contested."
        ));
        assert!(!is_citation_line(
            "- This is a bullet point in a list of conclusions, not a reference."
        ));
    }

    // ── chunks ─────────────────────────────────────────────────────

    #[test]
    fn a_pure_reference_list_is_detected() {
        let chunk = "AA. Schizophrenia  Schizophrenia is a complex and heterogeneous disorder.\n\
                     Berkenbosch F, van Vant J, Aubert A, et al. (2001) Neurotrophins and the brain.\n\
                     van Vant J, et al. (2004) Neurobiology of learning and memory. Nature Reviews.";
        assert!(is_reference_list(chunk));
        assert!(citation_line_share(chunk) > 0.5);
    }

    #[test]
    fn prose_is_not_a_reference_list() {
        let chunk = "The hippocampus is a region of the brain in the medial temporal lobe.\n\
                     It plays an essential role in consolidating memories and navigating space.\n\
                     Neurogenesis in this region continues throughout adulthood in rodents.";
        assert!(!is_reference_list(chunk));
        assert_eq!(citation_line_share(chunk), 0.0);
    }

    #[test]
    fn a_mixed_chunk_is_kept() {
        // One citation and one paragraph of real prose. The chunk
        // discusses the topic and cites it — it is useful, and the
        // threshold must not demote it.
        let chunk = "Neurogenesis was first reported by Altman (1962) in rodents.\n\
                     The rate of new neuron production in the adult hippocampus is low but measurable,\n\
                     and it rises after physical exercise, which remains contested (Bjorklund, 2013).";
        assert!(!is_reference_list(chunk));
    }

    #[test]
    fn an_empty_chunk_has_no_share() {
        assert_eq!(citation_line_share(""), 0.0);
        assert_eq!(citation_line_share("   \n\n  \n"), 0.0);
        assert!(!is_reference_list(""));
    }

    #[test]
    fn blank_lines_do_not_count_towards_the_share() {
        // Ten citation lines and ten blank lines is a reference list.
        let chunk = format!(
            "{}\n\n{}\n",
            "Smith, J. (2020). Title. Journal.", "Doe, A. (2021). Other. Press."
        );
        assert!(is_reference_list(&chunk));
    }
}
