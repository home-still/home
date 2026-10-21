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
    // "Sundralingham, P. 1983 Responses of potted seedlings of
    // Dryobalanops aromatica ..." — an author list and a **bare** year.
    // The rules above only see a year in parentheses, and a large part
    // of this corpus writes it bare: `Ferguson JN, Aldag JM, Insel TR,
    // Young LJ. Oxytocin in the medial amygdala is essential for social
    // recognition. J Neurosci 2009;29:1487-93.`
    if is_author_list_with_bare_year(s) {
        return true;
    }
    // "59. Ferguson JN, ..." / "(34) Lewis, J. A. ..." — a reference
    // number followed by an author. `is_numbered_entry` above only
    // strips `1.` and `2.`, because a number alone does not make a
    // citation: "22. The idea of the series is, in itself, important
    // to Eisenman's early development" is a numbered footnote.
    if is_numbered_author_entry(s) {
        return true;
    }
    // "- 5.Tinetti ME, Speechley M, Ginter SF. Risk factors for falls …"
    // — a bullet holding a *numbered* reference entry.
    if is_bulleted_numbered_author_entry(s) {
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

// ── the bare-year and numbered-author rules ──────────────────────────
//
// The rules above are *disjunctive*: one pattern is enough. That is
// what keeps them conservative, and it is also why they miss a large
// part of this corpus. The rules below are *conjunctive*: they need two
// signals at once — an author list **and** a year, or a reference
// number **and** an author — so they can afford a looser notion of
// "author list" without catching prose.
//
// Measured on the live corpus, 2026-10-21: the rules above scored the
// first hit of 6 of the 25 fixed queries at 0.00–0.43 — below the 0.5
// threshold — so those six queries opened with a bibliography. The
// shapes they missed all share the two signals below. See
// `docs/loop/runs/2026-10-21-06/research.md`.

/// Is there a year standing alone as a token?
///
/// A four-digit year between 1950 and 2029, bounded by whitespace or
/// punctuation, so that `2000>VPD` (a comparison) and `2013;44(4)756`
/// (a volume and a page range) do not count. The year must be in the
/// first 160 characters: a bibliography line names its year early, while
/// a prose sentence that happens to end with a year names it late.
///
/// A year that is *followed* by a volume and a page range is not one:
/// `Genes Dev. 2009, 23, 939-950` is a journal, a volume and a page
/// range, and the 2009 is the year of the volume — but the line is
/// already caught by the rules above, so the bare-year rule must not
/// claim it as well.
fn has_standalone_bare_year(s: &str) -> bool {
    let head: String = s.chars().take(160).collect();
    let bytes = head.as_bytes();
    let mut i = 0;
    while let Some(rel) = head[i..].find(|c: char| c.is_ascii_digit()) {
        let start = i + rel;
        // A year is four digits: the ones that are not four are a
        // measurement, a volume, a page number.
        let digits: String = head[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.len() == 4 {
            let year: u32 = digits.parse().unwrap_or(0);
            if (1950..=2029).contains(&year) {
                let before = if start == 0 {
                    ' '
                } else {
                    bytes[start - 1] as char
                };
                let after = bytes.get(start + 4).map(|b| *b as char).unwrap_or(' ');
                // The year is a token of its own: whitespace or
                // punctuation on both sides.
                let boundary = |c: char| {
                    c.is_whitespace() || matches!(c, ',' | '.' | ')' | '(' | '[' | ']' | ';' | ':')
                };
                if boundary(before) && boundary(after) {
                    // "Genes Dev. 2009, 23, 939-950" is a journal, a
                    // volume and a page range, not a bare year: the
                    // year is followed by a volume and a page range.
                    // The pattern is a year, a comma, a number, a
                    // comma, a page range.
                    let rest = &head[start + 4..];
                    let is_volume_then_pages = {
                        let mut parts = rest.trim_start_matches([' ', ',']).splitn(2, ',');
                        let first = parts.next().unwrap_or("").trim();
                        let second = parts.next().unwrap_or("");
                        first.chars().all(|c| c.is_ascii_digit())
                            && !first.is_empty()
                            && second.contains('-')
                            && second
                                .split('-')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .chars()
                                .all(|c| c.is_ascii_digit())
                    };
                    if !is_volume_then_pages {
                        return true;
                    }
                }
            }
        }
        // Move past this digit run.
        i = start + digits.len().max(1);
        if i >= head.len() {
            break;
        }
    }
    false
}

/// Does the line name its authors?
///
/// The author-list shape is the signal every referencing style shares —
/// the literature's own description of the standard heuristic ("look for
/// comma-separated author names at the start of the reference string",
/// arXiv 1912.07266). The styles that differ — initials, an ALL-CAPS
/// APA list, a full given name — are variations on it, so all four are
/// recognised here.
///
/// The scan is over **author pairs**, not over whitespace-separated
/// tokens: "Sundralingham, P." is a surname and an initial separated by
/// a comma, so a token that is split on the comma loses the pair.
fn has_author_list_shape(s: &str) -> bool {
    // "Ferguson JN, Aldag JM, Insel TR" — a capitalised surname followed
    // by one or two capitalised initials, with no comma between them.
    if s.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .any(is_surname_with_initials)
    {
        return true;
    }
    // "Sundralingham, P. 1983" — a capitalised surname, a comma, a
    // single initial. And the APA style, where the surname is ALL CAPS:
    // "ALBERG, D., LAST, M., AND KANDEL, A. 2012."
    //
    // The pair is read across the comma: the surname is what comes
    // before it and the initial what comes after.
    for (before, after) in s.split(',').zip(s.split(',').skip(1)) {
        let surname = before.split_whitespace().last().unwrap_or("");
        let surname = surname.trim_matches(|c: char| c == '(' || c == ')' || c == '.');
        if surname.is_empty() {
            continue;
        }
        let named = surname.chars().next().is_some_and(|c| c.is_uppercase());
        if !named {
            continue;
        }
        // The next token is a single initial: "P." or "D."
        let initial = after.split_whitespace().next().unwrap_or("");
        let initial = initial.trim_end_matches('.');
        if initial.len() == 1 && initial.chars().all(|c| c.is_uppercase()) {
            return true;
        }
    }
    // "Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua Bengio. 2015." — the
    // full given-name style: two capitalised words, then a comma.
    if let Some(pos) = s.find(", ") {
        let head = &s[..pos];
        let mut words = head.split_whitespace().rev();
        if let (Some(surname), Some(given)) = (words.next(), words.next()) {
            let is_name = |w: &str| {
                let core = w.trim_matches(|c: char| c == ',' || c == '.');
                !core.is_empty()
                    && core.chars().next().is_some_and(|c| c.is_uppercase())
                    && core.chars().filter(|c| c.is_alphabetic()).count() > 1
            };
            if is_name(surname) && is_name(given) {
                return true;
            }
        }
    }
    false
}

/// An author list and a bare year in the same line — the conjunction
/// that makes the rule safe.
///
/// "Sundralingham, P. 1983 Responses of potted seedlings of
/// Dryobalanops aromatica and Dryobalanops oblongifolia to shading."
///
/// A prose sentence has a year, or a number, but it does not have a
/// capitalised surname followed by initials; that is why both signals
/// are required.
fn is_author_list_with_bare_year(s: &str) -> bool {
    has_standalone_bare_year(s) && has_author_list_shape(s)
}

/// The text after a leading reference number, if there is one.
///
/// "(34) Lewis, J. A. ..." — a parenthesised number; "59. Ferguson JN,
/// ..." and "180 Daban C, ..." — a number followed by a stop or a space.
fn strip_reference_number(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('(') {
        let (number, rest) = rest.split_once(')')?;
        if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
            return Some(rest.trim_start());
        }
        return None;
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let after = &s[digits.len()..];
    let after = after.strip_prefix('.').unwrap_or(after);
    if after.is_empty() {
        return None;
    }
    Some(after.trim_start())
}

/// A reference number followed by an author-shaped token.
///
/// "59. Ferguson JN, Aldag JM, ..." and "(34) Lewis, J. A. Direct ink
/// writing of 3D functional materials."
///
/// The number alone is not enough: "22. The idea of the series is, in
/// itself, important to Eisenman's early development" is a numbered
/// footnote, and demoting it would be wrong. The token after the number
/// has to name an author.
fn is_numbered_author_entry(s: &str) -> bool {
    let rest = strip_reference_number(s);
    let Some(rest) = rest else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    // The first token names an author.
    let first = rest.split_whitespace().next().unwrap_or("");
    let first = first.trim_matches(|c: char| c == '(' || c == ')');
    if is_surname_with_initials(first) {
        return true;
    }
    // "Tinetti ME, …" / "Ferguson JN, …" — a surname and the initials
    // that follow it.
    if starts_with_surname_then_initials(rest) {
        return true;
    }
    // "Sundralingham, P." — a surname and a single initial.
    let mut parts = rest.split_whitespace();
    if let (Some(surname), Some(initial)) = (parts.next(), parts.next()) {
        let surname = surname.trim_end_matches(',');
        let initial = initial.trim_end_matches('.');
        if surname.chars().next().is_some_and(|c| c.is_uppercase())
            && initial.len() == 1
            && initial.chars().all(|c| c.is_uppercase())
        {
            return true;
        }
    }
    // "Eisenman, Peter." — a surname and a full given name.
    let mut parts = rest.splitn(2, ',');
    if let (Some(surname), Some(given)) = (parts.next(), parts.next()) {
        let given = given.trim_start();
        let given_word = given.split_whitespace().next().unwrap_or("");
        if surname.chars().next().is_some_and(|c| c.is_uppercase())
            && given_word.chars().next().is_some_and(|c| c.is_uppercase())
            && given_word.chars().filter(|c| c.is_alphabetic()).count() > 1
        {
            return true;
        }
    }
    // "Douglas R. Hofstadter, Gödel, Escher, Bach" — a given name, an
    // initial and a surname.
    let mut parts = rest.split_whitespace();
    if let (Some(given), Some(initial), Some(surname)) = (parts.next(), parts.next(), parts.next())
    {
        if given.chars().next().is_some_and(|c| c.is_uppercase())
            && initial.trim_end_matches('.').len() == 1
            && surname.chars().next().is_some_and(|c| c.is_uppercase())
        {
            return true;
        }
    }
    // "René Descartes, A Discourse on the Method, translated by …" —
    // a given name and a surname, then a comma. The pair is read
    // across the comma: the given name before it, the surname after.
    //
    // The comma is not necessarily the first one: "Robert E.
    // Ulanowicz, A Third Window: …" has the initial between the given
    // name and the surname, so the pair is looked for across *each* of
    // the first few commas.
    let is_name = |w: &str| {
        !w.is_empty()
            && w.chars().next().is_some_and(|c| c.is_uppercase())
            && w.chars().filter(|c| c.is_alphabetic()).count() > 1
    };
    let mut parts = rest.splitn(4, ',');
    let mut before = parts.next().unwrap_or("");
    for _ in 0..3 {
        let Some(after) = parts.next() else {
            break;
        };
        let given = before.split_whitespace().next().unwrap_or("");
        let surname = after.split_whitespace().next().unwrap_or("");
        if is_name(given) && is_name(surname) {
            return true;
        }
        before = after;
    }
    // "Douglas R. Hofstadter, Gödel, Escher, Bach: An Eternal Golden
    // Braid" — the given name and the surname are separated by an
    // initial, and the *surname* is the last word before the comma.
    if let Some((before, _)) = rest.split_once(',') {
        let words: Vec<&str> = before.split_whitespace().collect();
        if words.len() >= 3 {
            let given = words[0];
            let initial = words[1].trim_end_matches('.');
            let surname = words[2];
            if given.chars().next().is_some_and(|c| c.is_uppercase())
                && initial.len() == 1
                && initial.chars().all(|c| c.is_uppercase())
                && surname.chars().next().is_some_and(|c| c.is_uppercase())
            {
                return true;
            }
        }
    }
    // "1. 2002. A.L.I.C.E. Foundation website. (2002). Retrieved …" — a
    // numbered entry whose *year comes first*, with no author at all.
    // The number and the year are the signal: a reference list that
    // cites a web page names the year it was retrieved or published.
    // A numbered prose paragraph does not open with a year.
    if let Some(year) = rest.split_whitespace().next() {
        let year = year.trim_end_matches('.');
        if year.len() == 4 {
            if let Ok(y) = year.parse::<u32>() {
                if (1950..=2029).contains(&y) {
                    return true;
                }
            }
        }
    }
    false
}

/// A markdown bullet holding a *numbered* reference entry.
///
/// "- 5.Tinetti ME, Speechley M, Ginter SF. Risk factors for falls …"
///
/// The existing bulleted-author-list rule needs the bullet to be
/// followed by an author list, but this style puts a reference number
/// between the bullet and the authors. The number and the author list
/// are both required.
fn is_bulleted_numbered_author_entry(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("• "))
    else {
        return false;
    };
    let Some(rest) = strip_reference_number(rest) else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    // The text after the number names an author: "Tinetti ME, …" or
    // "Centers for Disease Control and Prevention, …".
    if starts_with_surname_then_initials(rest) {
        return true;
    }
    false
}

/// Does the text open with a surname followed by one or two initials?
///
/// "Tinetti ME, …" — a capitalised surname and the initials that
/// follow it, with no given name. The initials are one or two
/// capitalised letters: "Tinetti ME" and "Lewis JA" are both authors,
/// "Sundralingham P" is one.
fn starts_with_surname_then_initials(s: &str) -> bool {
    let mut parts = s.split_whitespace();
    let Some(surname) = parts.next() else {
        return false;
    };
    let surname = surname.trim_end_matches(',');
    if !surname.chars().next().is_some_and(|c| c.is_uppercase()) {
        return false;
    }
    let Some(initials) = parts.next() else {
        return false;
    };
    let initials = initials.trim_end_matches(',').trim_end_matches('.');
    (initials.len() == 1 || initials.len() == 2)
        && initials.chars().all(|c| c.is_uppercase())
        && !initials.is_empty()
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

    // ── the bare-year rule ─────────────────────────────────────────

    #[test]
    fn an_author_list_with_a_bare_year_is_a_citation() {
        // Real lines from this corpus, measured 2026-10-21. The year is
        // not in parentheses, which the rules above required.
        assert!(is_citation_line(
            "Sundralingham, P. 1983 Responses of potted seedlings of Dryobalanops aromatica \
             to shading. Malaysian Forester 46, 324-328."
        ));
        assert!(is_citation_line(
            "ALBERG, D., LAST, M., AND KANDEL, A. 2012. Knowledge discovery in data streams \
             with regression tree methods. Workshop on Data Streams."
        ));
        assert!(is_citation_line(
            "Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua Bengio. 2015. Neural machine \
             translation by jointly learning to align and translate. ICLR."
        ));
        assert!(is_citation_line(
            "Ferguson JN, Aldag JM, Insel TR, Young LJ. Oxytocin in the medial amygdala is \
             essential for social recognition. J Neurosci 2009;29:1487-93."
        ));
        assert!(is_citation_line(
            "Usunier, N.; Synnaeve, G.; Lin, Z.; and Chintala, S. 2016. Episodic exploration \
             for deep deterministic RL. ICLR."
        ));
    }

    #[test]
    fn a_bare_year_without_an_author_list_is_not_a_citation() {
        // The conjunction is the point: a year alone is prose.
        assert!(!is_citation_line(
            "Neurogenesis continues throughout adulthood in rodents, a finding first reported in 1962."
        ));
        assert!(!is_citation_line(
            "The metal content of the alloy was measured across all specimens in 1994 and 1995."
        ));
    }

    #[test]
    fn a_year_that_is_not_a_token_is_not_a_bare_year() {
        // A comparison, a volume and a page range: the digits are there
        // but they are not a year standing on its own.
        assert!(!has_standalone_bare_year(
            "the flow was 2000>VPD in all treatments"
        ));
        assert!(!has_standalone_bare_year(
            "Genes Dev. 2009, 23, 939-950. [CrossRef]"
        ));
        assert!(has_standalone_bare_year(
            "Sundralingham, P. 1983 Responses of seedlings."
        ));
    }

    #[test]
    fn a_year_outside_the_plausible_range_is_not_a_bare_year() {
        assert!(!has_standalone_bare_year(
            "The paper was cited 1200 times by 1847 readers"
        ));
        assert!(!has_standalone_bare_year(
            "Revision 2035 of the standard is pending"
        ));
    }

    // ── the numbered-author rule ───────────────────────────────────

    #[test]
    fn a_numbered_reference_entry_with_an_author_is_a_citation() {
        // The rules above only stripped "1." and "2."; this corpus
        // numbers its references up into the hundreds.
        assert!(is_citation_line(
            "59. Ferguson JN, Aldag JM, Insel TR, Young LJ. Oxytocin in the medial amygdala \
             is essential for social recognition."
        ));
        assert!(is_citation_line(
            "180 Daban C, Vieta E, Mackin P, Young AH. Hypothalamic-pituitary-adrenal axis \
             and bipolar disorder."
        ));
        assert!(is_citation_line(
            "(34) Lewis, J. A. Direct ink writing of 3D functional materials. Adv. Funct. Mater."
        ));
        assert!(is_citation_line(
            "22. Eisenman, Peter. House X. New York: Rizzoli International."
        ));
    }

    #[test]
    fn a_numbered_prose_line_is_not_a_citation() {
        // A number alone does not make a citation: this is a numbered
        // footnote, and demoting it would be wrong.
        assert!(!is_citation_line(
            "22. The idea of the series is, in itself, important to Eisenman's early development."
        ));
        assert!(!is_citation_line(
            "19. Eisenman is in an interesting tension with Bergson here, as he often was."
        ));
    }

    // ── the chunks the new rules catch ─────────────────────────────

    #[test]
    fn a_bibliography_in_the_bare_year_style_is_detected() {
        // The chunk that led the results for "nutrient limitation of
        // tropical forest growth" on 2026-10-21, scored 0.04 before.
        let chunk = "Sundralingham, P. 1983 Responses of potted seedlings of Dryobalanops \
                     aromatica to shading. Malaysian Forester.\n\
                     Tanner, E. V. J., Vitousek, P. M. & Cuevas, E. 1998 Experimental \
                     investigation of nutrient limitation. Ecology 79:744-52.\n\
                     Trenberth, K. E. 1997 The definition of El Nino. Bull. Am. Meteorol. Soc.";
        assert!(is_reference_list(chunk));
    }

    #[test]
    fn a_bibliography_in_the_numbered_style_is_detected() {
        // The chunk that led the results for "3D printing of controlled
        // release pharmaceutical bilayer tablets" on 2026-10-21, scored
        // 0.00 before.
        let chunk = "(32) Hull, C. W. Apparatus for production of three-dimensional objects \
                     by stereolithography. Patent US4575330A.\n\
                     (33) Lewis, J. A.; Smay, J. E. Direct ink writing of three-dimensional \
                     ceramic structures. J. Am. Ceram. Soc.\n\
                     (34) Lewis, J. A. Direct ink writing of 3D functional materials. Adv. \
                     Funct. Mater. 2006, 16 (17), 2193.";
        assert!(is_reference_list(chunk));
    }

    #[test]
    fn a_bibliography_in_the_apa_all_caps_style_is_detected() {
        let chunk = "ALBERG, D., LAST, M., AND KANDEL, A. 2012. Knowledge discovery in data \
                     streams with regression tree methods. Workshop on Data Streams.\n\
                     ANG, H., GOPALKRISHNAN, V., AND HOI, S. 2012. Predictive handling of \
                     asynchronous concurrent streams. PVLDB.\n\
                     BABCOCK, B., BABU, S., DATAR, M., MOTWANI, R., AND WIDOM, J. 2002. \
                     Models and issues in data stream systems. PODC.";
        assert!(is_reference_list(chunk));
    }
}
