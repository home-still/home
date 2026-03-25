/// Character Detection Metric (CDM) for formula evaluation.
/// Matches the official OmniDocBench v1.5 `normalized_formula()` normalization.
/// Strips only formatting commands (not structural ones like \frac, \alpha, \sum),
/// keeps braces, subscripts, and other structural LaTeX.
/// Score in [0, 100] where 100 means identical.

/// Score a single formula pair using token-level F1 matching.
///
/// Approximates official CDM: tokenize LaTeX → multiset intersection → F1.
/// Official CDM renders LaTeX to get per-token bounding boxes and uses
/// Hungarian matching with spatial alignment. We approximate by matching
/// tokens without spatial info (equivalent for most formulas).
pub fn cdm_score(reference_latex: &str, hypothesis_latex: &str) -> Option<f64> {
    let ref_clean = normalize_formula(reference_latex);
    let hyp_clean = normalize_formula(hypothesis_latex);

    if ref_clean.is_empty() && hyp_clean.is_empty() {
        return Some(100.0);
    }
    if ref_clean.is_empty() || hyp_clean.is_empty() {
        return Some(0.0);
    }

    // Token-level F1: tokenize both, compute multiset intersection
    let ref_tokens = tokenize_latex(&ref_clean);
    let hyp_tokens = tokenize_latex(&hyp_clean);

    if ref_tokens.is_empty() && hyp_tokens.is_empty() {
        return Some(100.0);
    }
    if ref_tokens.is_empty() || hyp_tokens.is_empty() {
        return Some(0.0);
    }

    // Build multiset counts
    let mut ref_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in &ref_tokens {
        *ref_counts.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut hyp_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in &hyp_tokens {
        *hyp_counts.entry(t.as_str()).or_insert(0) += 1;
    }

    // Multiset intersection: min count for each token
    let mut matches = 0usize;
    for (token, &ref_count) in &ref_counts {
        let hyp_count = hyp_counts.get(token).copied().unwrap_or(0);
        matches += ref_count.min(hyp_count);
    }

    let f1 = 2.0 * matches as f64 / (ref_tokens.len() + hyp_tokens.len()) as f64;
    Some((f1 * 100.0).min(100.0))
}

/// Tokenize LaTeX into individual tokens for CDM matching.
/// Splits on: \commands, single chars ({, }, ^, _, +, -, =, etc.), digits, letters.
fn tokenize_latex(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1].is_alphabetic() {
            // LaTeX command: \alpha, \frac, etc.
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        } else if chars[i].is_ascii_digit() {
            // Number: group consecutive digits
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        } else if chars[i].is_alphabetic() {
            // Single letter (variables are individual tokens in CDM)
            tokens.push(chars[i].to_string());
            i += 1;
        } else if !chars[i].is_whitespace() {
            // Operator or structural char: {, }, ^, _, +, -, =, (, ), etc.
            tokens.push(chars[i].to_string());
            i += 1;
        } else {
            i += 1; // skip whitespace
        }
    }

    tokens
}

/// Score multiple formulas per page using per-formula Hungarian matching.
/// Official OmniDocBench matches each ref formula to the best pred formula
/// individually, not as concatenated strings.
pub fn cdm_score_multi(ref_formulas: &[String], hyp_formulas: &[String]) -> Option<f64> {
    if ref_formulas.is_empty() {
        return None;
    }
    if hyp_formulas.is_empty() {
        return Some(0.0);
    }

    // Single formula fast path
    if ref_formulas.len() == 1 && hyp_formulas.len() == 1 {
        return cdm_score(&ref_formulas[0], &hyp_formulas[0]);
    }

    // Build NxM score matrix
    let mut scores: Vec<Vec<f64>> = Vec::new();
    for r in ref_formulas {
        let mut row = Vec::new();
        for h in hyp_formulas {
            let s = cdm_score(r, h).unwrap_or(0.0);
            row.push(s);
        }
        scores.push(row);
    }

    // Global best-first greedy matching (same as TEDS multi-table)
    let mut used_ref = vec![false; ref_formulas.len()];
    let mut used_hyp = vec![false; hyp_formulas.len()];
    let mut total_score = 0.0;
    let min_pairs = ref_formulas.len().min(hyp_formulas.len());

    for _ in 0..min_pairs {
        let mut best_score = -1.0;
        let mut best_i = 0;
        let mut best_j = 0;
        for (i, row) in scores.iter().enumerate() {
            if used_ref[i] { continue; }
            for (j, &s) in row.iter().enumerate() {
                if used_hyp[j] { continue; }
                if s > best_score {
                    best_score = s;
                    best_i = i;
                    best_j = j;
                }
            }
        }
        if best_score >= 0.0 {
            used_ref[best_i] = true;
            used_hyp[best_j] = true;
            total_score += best_score;
        }
    }
    // Unmatched ref formulas contribute 0

    Some(total_score / ref_formulas.len() as f64)
}

/// Official OmniDocBench v1.5 formula normalization.
/// Matches `normalized_formula()` from `utils/data_preprocess.py` exactly.
/// KEEPS structural LaTeX (\frac, \alpha, {, }, ^, _, operators).
/// Only strips formatting commands from the filter list.
fn normalize_formula(latex: &str) -> String {
    let mut s = latex.trim().to_string();

    // Strip $ delimiters (Python: text.strip('$') strips all leading/trailing $)
    s = s.trim_start_matches('$').trim_end_matches('$').to_string();
    // Strip leading/trailing newlines
    s = s.trim_matches('\n').to_string();

    // Strip BPE space markers (Ġ = U+0120) from model output (not in official, but needed for hypothesis)
    s = s.replace('\u{0120}', "");

    // Convert Unicode Greek to LaTeX commands for symmetric comparison
    s = unicode_greek_to_latex(&s);

    // Strip \[...\] display math wrapper
    s = strip_display_math(&s);

    // Strip \tag{...}, \hspace{...}, \begin{...}, \end{...}, \arraycolsep...}
    s = strip_brace_command(&s, "\\tag");
    s = strip_brace_command(&s, "\\hspace");
    s = strip_brace_command(&s, "\\begin");
    s = strip_brace_command(&s, "\\end");
    s = strip_arraycolsep(&s);

    // Strip trailing period
    s = s.trim_end_matches('.').to_string();

    // Strip formatting commands from the official filter list
    for cmd in FILTER_COMMANDS {
        s = s.replace(cmd, "");
    }

    // Official filter list also includes ' ' and '$$'
    s = s.replace(' ', "");
    s = s.replace("$$", "");

    s.to_lowercase()
}

/// Formatting commands stripped by the official OmniDocBench normalization.
/// These are pure formatting — structural commands like \frac, \alpha, \sum are KEPT.
const FILTER_COMMANDS: &[&str] = &[
    "\\mathbf", "\\mathrm", "\\mathnormal", "\\mathit", "\\mathbb",
    "\\mathcal", "\\mathscr", "\\mathfrak", "\\mathsf", "\\mathtt",
    "\\textbf", "\\text", "\\boldmath", "\\boldsymbol", "\\operatorname", "\\bm",
    "\\symbfit", "\\mathbfcal", "\\symbf", "\\scriptscriptstyle", "\\notag",
    "\\setlength", "\\coloneqq", "\\space", "\\thickspace", "\\thinspace",
    "\\medspace", "\\nobreakspace", "\\negmedspace",
    "\\quad", "\\qquad", "\\enspace", "\\substackw",
    "\\left", "\\right", "\\displaystyle",
];

/// Strip \[...\] display math wrapper, keeping inner content.
fn strip_display_math(s: &str) -> String {
    if let Some(start) = s.find("\\[") {
        if let Some(end) = s.rfind("\\]") {
            if end > start + 2 {
                return s[start + 2..end].trim().to_string();
            }
        }
    }
    s.to_string()
}

/// Strip \cmd{...} patterns (e.g., \tag{1}, \hspace{3pt}).
fn strip_brace_command(s: &str, cmd: &str) -> String {
    let mut result = s.to_string();
    loop {
        if let Some(pos) = result.find(cmd) {
            let after = &result[pos + cmd.len()..];
            if after.starts_with('{') {
                if let Some(close) = find_matching_brace(after) {
                    result = format!("{}{}", &result[..pos], &after[close + 1..]);
                    continue;
                }
            }
        }
        break;
    }
    result
}

/// Strip \arraycolsep...} pattern.
fn strip_arraycolsep(s: &str) -> String {
    let mut result = s.to_string();
    if let Some(pos) = result.find("\\arraycolsep") {
        if let Some(close) = result[pos..].find('}') {
            result = format!("{}{}", &result[..pos], &result[pos + close + 1..]);
        }
    }
    result
}

fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, ch) in s.chars().enumerate() {
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Map Unicode Greek letters to their LaTeX command form so both normalize identically.
fn unicode_greek_to_latex(s: &str) -> String {
    s.replace('α', "\\alpha").replace('β', "\\beta").replace('γ', "\\gamma")
     .replace('δ', "\\delta").replace('ε', "\\epsilon").replace('ζ', "\\zeta")
     .replace('η', "\\eta").replace('θ', "\\theta").replace('ι', "\\iota")
     .replace('κ', "\\kappa").replace('λ', "\\lambda").replace('μ', "\\mu")
     .replace('ν', "\\nu").replace('ξ', "\\xi").replace('π', "\\pi")
     .replace('ρ', "\\rho").replace('σ', "\\sigma").replace('τ', "\\tau")
     .replace('υ', "\\upsilon").replace('φ', "\\phi").replace('χ', "\\chi")
     .replace('ψ', "\\psi").replace('ω', "\\omega")
     // Uppercase
     .replace('Γ', "\\Gamma").replace('Δ', "\\Delta").replace('Θ', "\\Theta")
     .replace('Λ', "\\Lambda").replace('Π', "\\Pi").replace('Σ', "\\Sigma")
     .replace('Φ', "\\Phi").replace('Ψ', "\\Psi").replace('Ω', "\\Omega")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_formulas() {
        let score = cdm_score("E = mc^2", "E = mc^2").unwrap();
        assert_eq!(score, 100.0);
    }

    #[test]
    fn test_different_formulas() {
        let score = cdm_score("E = mc^2", "F = ma").unwrap();
        assert!(score < 100.0);
        assert!(score > 0.0);
        // Token F1: GT={e,=,m,c,^,2}, HYP={f,=,m,a} → matches={=,m}=2
        // F1 = 2*2/(6+4) = 0.4 → 40.0
        assert!((score - 40.0).abs() < 1.0, "expected ~40, got {}", score);
    }

    #[test]
    fn test_tokenize_latex() {
        let tokens = tokenize_latex("\\frac{a}{b}+c");
        assert_eq!(tokens, vec!["\\frac", "{", "a", "}", "{", "b", "}", "+", "c"]);

        let tokens = tokenize_latex("x^{2}+y");
        assert_eq!(tokens, vec!["x", "^", "{", "2", "}", "+", "y"]);

        let tokens = tokenize_latex("\\alpha+\\beta=\\gamma");
        assert_eq!(tokens, vec!["\\alpha", "+", "\\beta", "=", "\\gamma"]);
    }

    #[test]
    fn test_with_dollar_signs() {
        let score = cdm_score("$$E = mc^2$$", "E = mc^2").unwrap();
        assert_eq!(score, 100.0);
    }

    #[test]
    fn test_normalize_keeps_structure() {
        // Structural commands should be KEPT (official method)
        let n = normalize_formula("\\frac{a}{b}");
        assert!(n.contains("\\frac"), "got: {}", n);
        let n = normalize_formula("\\alpha + \\beta");
        assert!(n.contains("\\alpha"), "got: {}", n);
        let n = normalize_formula("\\sum_{i=1}^{n}");
        assert!(n.contains("\\sum"), "got: {}", n);
    }

    #[test]
    fn test_normalize_strips_formatting() {
        // Formatting commands should be stripped (official method)
        let n = normalize_formula("\\mathbf{x}");
        assert!(!n.contains("\\mathbf"), "got: {}", n);
        assert!(n.contains("{x}"), "got: {}", n);
        let n = normalize_formula("\\text{hello}");
        assert!(!n.contains("\\text"), "got: {}", n);
        assert!(n.contains("{hello}"), "got: {}", n);
    }

    #[test]
    fn test_normalize_keeps_operators_and_braces() {
        // Official normalization keeps structural chars: {, }, ^, _, +, -, =
        let n = normalize_formula("\\frac{x^2}{y+1}");
        assert!(n.contains("{"), "should keep braces, got: {}", n);
        assert!(n.contains("^"), "should keep caret, got: {}", n);
        assert!(n.contains("+"), "should keep plus, got: {}", n);
    }

    #[test]
    fn test_normalize_strips_spaces() {
        // Official filter list includes ' ' — all spaces removed
        let n = normalize_formula("a + b = c");
        assert!(!n.contains(' '), "should strip spaces, got: '{}'", n);
        assert_eq!(n, "a+b=c");
    }

    #[test]
    fn test_greek_symmetry() {
        // Unicode Greek and LaTeX commands must normalize identically
        assert_eq!(normalize_formula("\\alpha + \\beta"), normalize_formula("α + β"));
        assert_eq!(normalize_formula("\\Gamma"), normalize_formula("Γ"));
    }

    #[test]
    fn test_tag_stripping() {
        let n = normalize_formula("x \\tag{1}");
        assert!(!n.contains("\\tag"), "got: {}", n);
        assert!(n.contains("x"), "got: {}", n);
    }

    #[test]
    fn test_begin_end_stripping() {
        let s = normalize_formula("\\begin{aligned} x = 1 \\end{aligned}");
        assert!(!s.contains("\\begin"), "got: {}", s);
        assert!(!s.contains("\\end"), "got: {}", s);
        assert!(s.contains("x"), "got: {}", s);
    }

    #[test]
    fn test_operatorname_stripped() {
        // \operatorname is in the filter list
        let n = normalize_formula("\\operatorname{sin}(x)");
        assert!(!n.contains("\\operatorname"), "got: {}", n);
        assert!(n.contains("{sin}(x)"), "got: {}", n);
    }
}
