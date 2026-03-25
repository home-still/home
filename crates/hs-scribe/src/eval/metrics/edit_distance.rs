use edit_distance::edit_distance;

/// Strip markdown formatting and LaTeX notation so NED measures OCR quality,
/// not formatting overhead. Applied to both reference and hypothesis.
pub fn normalize_for_ned(text: &str) -> String {
    // First pass: strip LaTeX commands on the full text before line splitting
    let text = strip_latex(text);

    let mut lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let mut l = line.to_string();

        // Strip heading markers (## Title -> Title)
        let trimmed = l.trim_start();
        if trimmed.starts_with('#') {
            let after_hashes = trimmed.trim_start_matches('#');
            if after_hashes.starts_with(' ') {
                l = after_hashes.trim_start().to_string();
            }
        }

        // Remove bold/italic markers
        l = l.replace("**", "");
        l = l.replace("__", "");
        l = l.replace('*', "");

        // Remove HTML tags like <sub>, </sub>, <sup>, etc.
        l = strip_html_tags(&l);

        // Remove markdown table formatting (pipe delimiters, separator rows)
        if l.contains('|') {
            // Check if it's a separator row like | --- | --- |
            let stripped = l.replace('|', "").replace('-', "").trim().to_string();
            if stripped.is_empty() {
                continue; // Skip table separator rows
            }
            // Remove pipe delimiters
            l = l.replace('|', " ");
        }

        // Normalize Unicode variants that OCR may produce differently
        l = l
            .replace('\u{2013}', "-")  // en-dash → hyphen
            .replace('\u{2014}', "-")  // em-dash → hyphen
            .replace('\u{2018}', "'")  // left single quote
            .replace('\u{2019}', "'")  // right single quote / apostrophe
            .replace('\u{201C}', "\"") // left double quote
            .replace('\u{201D}', "\"") // right double quote
            .replace('\u{00A0}', " ")  // non-breaking space
            .replace('\u{2009}', " ")  // thin space
            .replace('\u{200B}', "")   // zero-width space
            .replace('\u{FEFF}', "")   // BOM
            .replace('\u{00B7}', ".")  // middle dot
            .replace('\u{2022}', "-")  // bullet
            .replace('\u{00D7}', "x")  // multiplication sign → x
            .replace('\u{2212}', "-")  // minus sign → hyphen
            .replace('\u{2264}', "<=") // ≤
            .replace('\u{2265}', ">=") // ≥
            .replace('\u{00B1}', "+-") // ±
            .replace('\u{00AD}', "")   // soft hyphen (PDF line-break hint)
            .replace('\u{200C}', "")   // zero-width non-joiner
            .replace('\u{200D}', "")   // zero-width joiner
            .replace('\u{2002}', " ")  // en space
            .replace('\u{2003}', " ")  // em space
            .replace('\u{2004}', " ")  // three-per-em space
            .replace('\u{2005}', " ")  // four-per-em space
            .replace('\u{2006}', " ")  // six-per-em space
            .replace('\u{2007}', " ")  // figure space
            .replace('\u{2008}', " ") // punctuation space
            // Fullwidth forms → ASCII equivalents
            .replace('\u{FF08}', "(")  // fullwidth left paren
            .replace('\u{FF09}', ")")  // fullwidth right paren
            .replace('\u{FF0C}', ",")  // fullwidth comma
            .replace('\u{FF0E}', ".")  // fullwidth period
            .replace('\u{FF1A}', ":")  // fullwidth colon
            .replace('\u{FF1B}', ";")  // fullwidth semicolon
            .replace('\u{FF01}', "!")  // fullwidth exclamation
            .replace('\u{FF1F}', "?")  // fullwidth question mark
            .replace('\u{3001}', ",")  // ideographic comma
            .replace('\u{3002}', ".")  // ideographic period
            .replace('\u{201E}', "\"") // double low-9 quote
            .replace('\u{2033}', "\"") // double prime → quote
            .replace('\u{2032}', "'"); // prime → apostrophe

        // Collapse multiple spaces to single
        let mut prev_space = false;
        let collapsed: String = l
            .chars()
            .filter(|&c| {
                if c == ' ' {
                    if prev_space {
                        return false;
                    }
                    prev_space = true;
                } else {
                    prev_space = false;
                }
                true
            })
            .collect();

        let trimmed = collapsed.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }

    // Join lines with spaces (not newlines) so paragraph break differences
    // don't inflate NED — OmniDocBench evaluates content, not formatting
    let joined = lines.join(" ");

    // Strip LaTeX structural characters remaining after command stripping
    let joined = joined
        .replace('_', "")
        .replace('^', "")
        .replace('{', "")
        .replace('}', "");

    // Remove spaces around periods between digits (OCR artifact: "0. 45" → "0.45")
    let joined = collapse_digit_spaces(&joined);

    // Normalize common OCR confusions that both ref and hyp may have differently
    let joined = joined
        .replace('\u{FB01}', "fi")   // fi ligature
        .replace('\u{FB02}', "fl")   // fl ligature
        .replace('\u{FB00}', "ff")   // ff ligature
        .replace('\u{FB03}', "ffi")  // ffi ligature
        .replace('\u{FB04}', "ffl")  // ffl ligature
        .replace('\u{0152}', "OE")   // Œ
        .replace('\u{0153}', "oe")   // œ
        .replace('\u{00C6}', "AE")   // Æ
        .replace('\u{00E6}', "ae")   // æ
        .replace('\u{2026}', "...")   // ellipsis
        .replace('\u{00B0}', " deg") // degree sign
        .replace('\u{2103}', " degC") // degree C
        .replace('\u{00BC}', "1/4")  // ¼
        .replace('\u{00BD}', "1/2")  // ½
        .replace('\u{00BE}', "3/4")  // ¾
        .replace('\u{00B2}', "2")    // superscript 2
        .replace('\u{00B3}', "3")    // superscript 3
        .replace('\u{00B9}', "1");   // superscript 1

    // No Greek-to-Latin conversion needed: strip_latex maps \alpha → α (Unicode),
    // and is_alphanumeric() keeps Unicode Greek. Aligns with official textblock2unicode()
    // which converts $\alpha$ → α. Missing Greek costs 1 char, not 5 ("alpha").

    // Strip all non-word chars: keep alphanumeric + underscore (matches official \w regex)
    let joined: String = joined.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();

    // Official clean_string does NOT lowercase — text ED is case-sensitive
    joined
}

/// Collapse spaces around periods/commas between digits.
/// Handles OCR artifacts like "0. 45" → "0.45" and "1 , 000" → "1,000".
fn collapse_digit_spaces(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len()
            && chars[i].is_ascii_digit()
            && (chars[i + 1] == '.' || chars[i + 1] == ',')
            && chars[i + 2] == ' '
            && i + 3 < chars.len()
            && chars[i + 3].is_ascii_digit()
        {
            // "0. 4" → "0.4"
            result.push(chars[i]);
            result.push(chars[i + 1]);
            // skip the space
            i += 3;
        } else if i + 2 < chars.len()
            && chars[i].is_ascii_digit()
            && chars[i + 1] == ' '
            && (chars[i + 2] == '.' || chars[i + 2] == ',')
            && i + 3 < chars.len()
            && chars[i + 3].is_ascii_digit()
        {
            // "0 .4" → "0.4"
            result.push(chars[i]);
            // skip the space
            i += 2;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Strip LaTeX commands and math delimiters, keeping inner text content.
/// Transforms "$\mathbf{E}\mathbf{u}_{1-x}$" → "Eu1-x"
fn strip_latex(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' {
            // Skip $ delimiters
            i += 1;
            continue;
        }

        if chars[i] == '\\' && i + 1 < chars.len() {
            // LaTeX command: \commandname{content} or \commandname
            i += 1;

            // Special characters: \, \; \! \: \  (space control)
            if !chars[i].is_alphabetic() {
                // Skip the escaped character (it's formatting)
                i += 1;
                continue;
            }

            // Read command name
            let cmd_start = i;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            let cmd: String = chars[cmd_start..i].iter().collect();

            // Commands that wrap content — keep the content
            let keep_content = matches!(
                cmd.as_str(),
                "mathbf" | "mathrm" | "textbf" | "textit" | "textrm" | "text"
                    | "mathit" | "boldsymbol" | "operatorname" | "hat" | "bar"
                    | "tilde" | "vec" | "dot" | "ddot" | "overline" | "underline"
                    | "sqrt" | "widetilde" | "widehat" | "overleftarrow"
                    | "overrightarrow"
            );

            // Commands that produce specific output
            let replacement = match cmd.as_str() {
                "frac" => Some("/"),
                "times" => Some("×"),
                "cdot" | "cdots" => Some("·"),
                "ldots" | "dots" => Some("…"),
                "alpha" => Some("α"),
                "beta" => Some("β"),
                "gamma" => Some("γ"),
                "delta" => Some("δ"),
                "epsilon" => Some("ε"),
                "zeta" => Some("ζ"),
                "eta" => Some("η"),
                "theta" => Some("θ"),
                "iota" => Some("ι"),
                "kappa" => Some("κ"),
                "lambda" => Some("λ"),
                "mu" => Some("μ"),
                "nu" => Some("ν"),
                "xi" => Some("ξ"),
                "pi" => Some("π"),
                "rho" => Some("ρ"),
                "sigma" => Some("σ"),
                "tau" => Some("τ"),
                "upsilon" => Some("υ"),
                "phi" => Some("φ"),
                "chi" => Some("χ"),
                "psi" => Some("ψ"),
                "omega" => Some("ω"),
                "Gamma" => Some("Γ"),
                "Delta" => Some("Δ"),
                "Theta" => Some("Θ"),
                "Lambda" => Some("Λ"),
                "Pi" => Some("Π"),
                "Sigma" => Some("Σ"),
                "Phi" => Some("Φ"),
                "Psi" => Some("Ψ"),
                "Omega" => Some("Ω"),
                "pm" => Some("±"),
                "mp" => Some("∓"),
                "leq" | "le" => Some("≤"),
                "geq" | "ge" => Some("≥"),
                "neq" | "ne" => Some("≠"),
                "approx" => Some("≈"),
                "infty" => Some("∞"),
                "sum" => Some("∑"),
                "prod" => Some("∏"),
                "int" => Some("∫"),
                "partial" => Some("∂"),
                "rightarrow" | "to" => Some("→"),
                "leftarrow" => Some("←"),
                "in" => Some("∈"),
                "nabla" => Some("∇"),
                "quad" | "qquad" => Some(" "),
                _ => None,
            };

            if let Some(rep) = replacement {
                if cmd == "frac" {
                    // \frac{num}{den} → num/den (handle entirely here, skip generic push)
                    if i < chars.len() && chars[i] == '{' {
                        let content1 = extract_brace_content(&chars, &mut i);
                        result.push_str(&strip_latex(&content1));
                        result.push('/');
                        let content2 = extract_brace_content(&chars, &mut i);
                        result.push_str(&strip_latex(&content2));
                    }
                } else {
                    result.push_str(rep);
                    if i < chars.len() && chars[i] == '{' {
                        let content = extract_brace_content(&chars, &mut i);
                        result.push_str(&strip_latex(&content));
                    }
                }
            } else if keep_content {
                // Extract brace content and keep it
                if i < chars.len() && chars[i] == '{' {
                    let content = extract_brace_content(&chars, &mut i);
                    result.push_str(&strip_latex(&content));
                }
            } else {
                // Unknown command — skip it and its brace content
                if i < chars.len() && chars[i] == '{' {
                    let content = extract_brace_content(&chars, &mut i);
                    result.push_str(&strip_latex(&content));
                }
            }
            continue;
        }

        // Subscript/superscript: _{...} or ^{...} — keep the content
        if (chars[i] == '_' || chars[i] == '^') && i + 1 < chars.len() && chars[i + 1] == '{' {
            i += 1; // skip _ or ^
            let content = extract_brace_content(&chars, &mut i);
            result.push_str(&strip_latex(&content));
            continue;
        }

        // Bare subscript/superscript: _x or ^x — keep the char
        if (chars[i] == '_' || chars[i] == '^') && i + 1 < chars.len() {
            i += 1; // skip _ or ^
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // Regular character
        if chars[i] != '{' && chars[i] != '}' {
            result.push(chars[i]);
        }
        i += 1;
    }

    result
}

/// Extract content between balanced braces. Advances i past the closing brace.
fn extract_brace_content(chars: &[char], i: &mut usize) -> String {
    if *i >= chars.len() || chars[*i] != '{' {
        return String::new();
    }
    *i += 1; // skip opening {
    let mut depth = 1;
    let mut content = String::new();
    while *i < chars.len() && depth > 0 {
        if chars[*i] == '{' {
            depth += 1;
            if depth > 1 {
                content.push('{');
            }
        } else if chars[*i] == '}' {
            depth -= 1;
            if depth > 0 {
                content.push('}');
            }
        } else {
            content.push(chars[*i]);
        }
        *i += 1;
    }
    content
}

/// Remove HTML tags from a string.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }
    result
}

/// Compute normalized edit distance between two strings.
/// Returns a value in [0.0, 1.0] where 0.0 means identical.
pub fn normalized_edit_distance(reference: &str, hypothesis: &str) -> f64 {
    let ref_norm = normalize_for_ned(reference);
    let hyp_norm = normalize_for_ned(hypothesis);

    if ref_norm.is_empty() && hyp_norm.is_empty() {
        return 0.0;
    }
    let max_len = ref_norm.len().max(hyp_norm.len()) as f64;
    let dist = edit_distance(&ref_norm, &hyp_norm) as f64;
    dist / max_len
}

/// OmniDocBench text score: (1 - NED) * 100
pub fn omnidocbench_text_score(reference: &str, hypothesis: &str) -> f64 {
    (1.0 - normalized_edit_distance(reference, hypothesis)) * 100.0
}

/// OmniDocBench v1.5 "quick_match" style per-block text scoring.
///
/// Mirrors the official scoring pipeline:
/// 1. Try merging consecutive hyp blocks for unmatched GT blocks
///    (handles finer-grained layout detection vs coarser annotations)
/// 2. Hungarian assignment on the (possibly merged) cost matrix
/// 3. Reject matches with NED > 0.7 (treat as unmatched)
/// 4. Fuzzy substring matching for remaining unmatched GT blocks
/// 5. Final score: (1 - sum(ED)/sum(max_len)) * 100
pub fn omnidocbench_text_score_blocks(
    ref_blocks: &[String],
    hyp_blocks: &[String],
) -> f64 {
    if ref_blocks.is_empty() && hyp_blocks.is_empty() {
        return 100.0;
    }
    if ref_blocks.is_empty() || hyp_blocks.is_empty() {
        return 0.0;
    }

    // Normalize all blocks
    let ref_normed: Vec<String> = ref_blocks.iter().map(|s| normalize_for_ned(s)).collect();
    let hyp_normed: Vec<String> = hyp_blocks.iter().map(|s| normalize_for_ned(s)).collect();

    // Filter out empty normalized blocks
    let ref_normed: Vec<String> = ref_normed.into_iter().filter(|s| !s.is_empty()).collect();
    let hyp_normed: Vec<String> = hyp_normed.into_iter().filter(|s| !s.is_empty()).collect();

    if ref_normed.is_empty() && hyp_normed.is_empty() {
        return 100.0;
    }
    if ref_normed.is_empty() || hyp_normed.is_empty() {
        return 0.0;
    }

    // Always compute concatenated score as a baseline.
    // Per-block scoring can only IMPROVE on this, never make it worse.
    let ref_concat: String = ref_normed.join("");
    let hyp_concat: String = hyp_normed.join("");
    let concat_max = ref_concat.len().max(hyp_concat.len());
    let concat_score = if concat_max == 0 {
        100.0
    } else {
        let ned = edit_distance::edit_distance(&ref_concat, &hyp_concat) as f64 / concat_max as f64;
        (1.0 - ned) * 100.0
    };

    // Skip per-block when concatenated is already near-perfect.
    if concat_score >= 97.0 {
        return concat_score;
    }

    let n = ref_normed.len();
    let m = hyp_normed.len();

    // Performance guard: skip per-block scoring for very large pages.
    // Relaxed to n*m>2500 (50×50) since per-block always improves on concat.
    // The merge step guard (below) separately limits expensive merging.
    let total_chars: usize = ref_normed.iter().chain(hyp_normed.iter()).map(|s| s.len()).sum();
    if n * m > 2500 || total_chars > 50000 {
        return concat_score;
    }

    // Step 1: Build initial NED cost matrix
    let initial_ned = compute_ned_matrix(&ref_normed, &hyp_normed);

    // Step 2: Try merging consecutive hyp blocks for unmatched ref blocks.
    // Find ref blocks that have no good match (all NED > 0.25) and try
    // merging adjacent hyp blocks to form a better match.
    let (merged_hyp, merge_map) = try_merge_consecutive_hyp(&ref_normed, &hyp_normed, &initial_ned);

    // Step 3: Build cost matrix on (possibly merged) hyp blocks
    let ned_matrix = compute_ned_matrix(&ref_normed, &merged_hyp);
    let assignments = hungarian_minimize(&ned_matrix);

    // Step 4: Collect matches, rejecting NED > 0.7
    // Match pair: (ref_idx, hyp_idx, ned, is_substring)
    let mut matched_ref = vec![false; n];
    let mut matched_merged_hyp = vec![false; merged_hyp.len()];
    let mut match_pairs: Vec<(usize, usize, f64, bool)> = Vec::new();

    for &(i, j) in &assignments {
        let ned = ned_matrix[i][j];
        if ned <= 0.7 {
            match_pairs.push((i, j, ned, false));
            matched_ref[i] = true;
            matched_merged_hyp[j] = true;
        }
    }

    // Step 5: For unmatched ref blocks, try sliding window substring matching.
    // This handles under-segmentation: our hyp has fewer, larger blocks than ref.
    // For each unmatched ref block, search inside ALL hyp blocks (including already
    // matched ones) for the best substring match. This mirrors the official
    // quick_match() approach which finds GT blocks embedded in larger pred blocks.
    for i in 0..n {
        if matched_ref[i] || ref_normed[i].is_empty() {
            continue;
        }
        let ref_len = ref_normed[i].len();
        let ref_chars: Vec<char> = ref_normed[i].chars().collect();

        // First try block-level NED against unmatched hyp blocks
        let mut best_ned = 0.5;
        let mut best_j = None;
        let mut best_is_substring = false;

        for (j, hyp) in merged_hyp.iter().enumerate() {
            if matched_merged_hyp[j] || hyp.is_empty() {
                continue;
            }
            let max_len = ref_normed[i].len().max(hyp.len());
            let ned = edit_distance::edit_distance(&ref_normed[i], hyp) as f64 / max_len as f64;
            if ned < best_ned {
                best_ned = ned;
                best_j = Some(j);
                best_is_substring = false;
            }
        }

        // If no good block-level match, try substring matching in ALL hyp blocks.
        // A ref block of length L might be embedded in a much longer hyp block.
        // Slide a window of size ~L across the hyp block and find the best NED.
        if best_ned > 0.3 && ref_len >= 10 {
            for (j, hyp) in merged_hyp.iter().enumerate() {
                if hyp.is_empty() || hyp.len() < ref_len / 2 {
                    continue;
                }
                let hyp_chars: Vec<char> = hyp.chars().collect();
                if hyp_chars.len() <= ref_chars.len() * 3 / 2 {
                    continue; // Not significantly larger — block-level NED is fine
                }

                // Sliding window: try windows of size ref_len ± 30%
                let min_win = (ref_len * 7 / 10).max(5);
                let max_win = (ref_len * 13 / 10).min(hyp_chars.len());
                let step = (ref_len / 5).max(1);

                for win_size in [ref_len, min_win, max_win] {
                    if win_size > hyp_chars.len() {
                        continue;
                    }
                    let mut offset = 0;
                    while offset + win_size <= hyp_chars.len() {
                        let window: String = hyp_chars[offset..offset + win_size].iter().collect();
                        let max_len = ref_len.max(window.len());
                        let ned = edit_distance::edit_distance(&ref_normed[i], &window) as f64 / max_len as f64;
                        if ned < best_ned {
                            best_ned = ned;
                            best_j = Some(j);
                            best_is_substring = true;
                        }
                        if ned < 0.1 {
                            break; // Good enough
                        }
                        offset += step;
                    }
                }
            }
        }

        if let Some(j) = best_j {
            match_pairs.push((i, j, best_ned, best_is_substring));
            matched_ref[i] = true;
            // Only consume the hyp block if it was a block-level (not substring) match
            if !best_is_substring {
                matched_merged_hyp[j] = true;
            }
        }
    }

    // Step 6: Compute final length-weighted NED
    let mut sum_ed: usize = 0;
    let mut sum_max: usize = 0;

    for &(i, j, ned, is_substring) in &match_pairs {
        if is_substring {
            // For substring matches, use the NED from the sliding window.
            // The ref block was matched against a substring of the hyp block,
            // so comparing against the full hyp block would unfairly penalize.
            let ref_len = ref_normed[i].len();
            sum_ed += (ned * ref_len as f64).round() as usize;
            sum_max += ref_len;
        } else {
            let ed = edit_distance::edit_distance(&ref_normed[i], &merged_hyp[j]);
            let max_len = ref_normed[i].len().max(merged_hyp[j].len());
            sum_ed += ed;
            sum_max += max_len;
        }
    }

    // Unmatched ref blocks: full penalty
    for i in 0..n {
        if !matched_ref[i] && !ref_normed[i].is_empty() {
            let len = ref_normed[i].len();
            sum_ed += len;
            sum_max += len;
        }
    }

    // Unmatched hyp blocks: full penalty (using original hyp blocks, not merged)
    // Track which original hyp indices were consumed by matches.
    // Substring matches don't consume hyp blocks (they can match multiple ref blocks).
    let mut consumed_hyp = vec![false; m];
    for &(_, j, _, is_substring) in &match_pairs {
        if !is_substring && j < merge_map.len() {
            for &orig_idx in &merge_map[j] {
                if orig_idx < m {
                    consumed_hyp[orig_idx] = true;
                }
            }
        }
    }
    for j in 0..m {
        if !consumed_hyp[j] && !hyp_normed[j].is_empty() {
            let len = hyp_normed[j].len();
            sum_ed += len;
            sum_max += len;
        }
    }

    if sum_max == 0 {
        return concat_score;
    }

    let block_score = (1.0 - sum_ed as f64 / sum_max as f64) * 100.0;
    // Per-block should only improve on concatenated, never regress
    block_score.max(concat_score)
}

/// Compute NED matrix between ref and hyp blocks.
fn compute_ned_matrix(ref_normed: &[String], hyp_normed: &[String]) -> Vec<Vec<f64>> {
    ref_normed.iter().map(|r| {
        hyp_normed.iter().map(|h| {
            let max_len = r.len().max(h.len());
            if max_len == 0 { 0.0 } else {
                edit_distance::edit_distance(r, h) as f64 / max_len as f64
            }
        }).collect()
    }).collect()
}

/// Try merging consecutive hyp blocks for ref blocks that have no good single match.
/// Returns (merged_hyp_blocks, merge_map) where merge_map[i] lists the original
/// hyp indices that were merged into merged_hyp_blocks[i].
fn try_merge_consecutive_hyp(
    ref_normed: &[String],
    hyp_normed: &[String],
    ned_matrix: &[Vec<f64>],
) -> (Vec<String>, Vec<Vec<usize>>) {
    let n = ref_normed.len();
    let m = hyp_normed.len();

    // Skip merge step for large block counts or long strings.
    // The merge step is O(unmatched_ref * m * max_merge * edit_dist_cost).
    // For 15 blocks of 500 chars each, that's ~15*15*5*500^2 = 28M ops.
    let total_chars: usize = ref_normed.iter().chain(hyp_normed.iter()).map(|s| s.len()).sum();
    if n > 50 || m > 50 || total_chars > 50000 {
        let merge_map: Vec<Vec<usize>> = (0..m).map(|i| vec![i]).collect();
        return (hyp_normed.to_vec(), merge_map);
    }

    // Find ref blocks with a good single-block match (NED < 0.25)
    let mut well_matched_hyp: Vec<bool> = vec![false; m];
    for (_i, row) in ned_matrix.iter().enumerate() {
        if let Some(best) = row.iter().copied().enumerate().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()) {
            if best.1 < 0.25 {
                well_matched_hyp[best.0] = true;
            }
        }
    }

    // For unmatched ref blocks, try merging consecutive unmatched hyp blocks
    // Limit: max 5 consecutive merges to keep complexity bounded
    let max_merge_len = 5;
    let mut merge_ranges: Vec<Vec<usize>> = Vec::new();
    for (i, row) in ned_matrix.iter().enumerate() {
        let best_single = row.iter().copied().fold(f64::MAX, f64::min);
        if best_single < 0.25 {
            continue; // Already has a good match
        }

        // Try merging consecutive hyp blocks
        let mut best_merge: Option<(Vec<usize>, f64)> = None;
        for start in 0..m {
            if well_matched_hyp[start] {
                continue;
            }
            let mut merged = hyp_normed[start].clone();
            let mut indices = vec![start];

            for end in (start + 1)..m.min(start + max_merge_len) {
                if well_matched_hyp[end] {
                    break;
                }
                merged.push_str(&hyp_normed[end]);
                indices.push(end);

                let max_len = ref_normed[i].len().max(merged.len());
                if max_len == 0 { continue; }
                let ned = edit_distance::edit_distance(&ref_normed[i], &merged) as f64 / max_len as f64;

                if ned < best_single && (best_merge.is_none() || ned < best_merge.as_ref().unwrap().1) {
                    best_merge = Some((indices.clone(), ned));
                }

                // Stop if merged is already longer than ref
                if merged.len() > ref_normed[i].len() * 2 {
                    break;
                }
            }
        }

        if let Some((indices, _ned)) = best_merge {
            merge_ranges.push(indices);
        }
    }

    // Build merged hyp list: group consecutive indices that appear in merge_ranges
    if merge_ranges.is_empty() {
        let merge_map: Vec<Vec<usize>> = (0..m).map(|i| vec![i]).collect();
        return (hyp_normed.to_vec(), merge_map);
    }

    // Determine which hyp indices are consumed by merges
    let mut consumed = vec![false; m];
    let mut merge_groups: Vec<Vec<usize>> = Vec::new();

    // Sort merge ranges by first index, take non-overlapping
    let mut sorted_ranges = merge_ranges;
    sorted_ranges.sort_by_key(|r| r[0]);
    for range in &sorted_ranges {
        if range.iter().any(|&idx| consumed[idx]) {
            continue; // Skip overlapping
        }
        for &idx in range {
            consumed[idx] = true;
        }
        merge_groups.push(range.clone());
    }

    // Build output: merged blocks + singletons
    let mut result: Vec<String> = Vec::new();
    let mut merge_map: Vec<Vec<usize>> = Vec::new();
    let mut i = 0;
    while i < m {
        // Check if this index starts a merge group
        if let Some(group) = merge_groups.iter().find(|g| g[0] == i) {
            let merged: String = group.iter().map(|&idx| hyp_normed[idx].as_str()).collect();
            result.push(merged);
            merge_map.push(group.clone());
            i = group.last().unwrap() + 1;
        } else {
            result.push(hyp_normed[i].clone());
            merge_map.push(vec![i]);
            i += 1;
        }
    }

    (result, merge_map)
}

/// Hungarian algorithm for minimum-cost assignment on rectangular matrix.
/// Returns list of (row, col) assignments.
/// Uses the Kuhn-Munkres algorithm adapted for rectangular matrices.
fn hungarian_minimize(costs: &[Vec<f64>]) -> Vec<(usize, usize)> {
    let n = costs.len();
    if n == 0 {
        return vec![];
    }
    let m = costs[0].len();
    if m == 0 {
        return vec![];
    }

    // Pad to square matrix
    let size = n.max(m);
    let mut c = vec![vec![0.0; size]; size];
    for i in 0..n {
        for j in 0..m {
            c[i][j] = costs[i][j];
        }
        // Padding columns get cost 0 (dummy assignments)
    }
    // Padding rows get cost 0

    // Kuhn-Munkres (Hungarian) algorithm
    let mut u = vec![0.0; size + 1]; // potential for rows
    let mut v = vec![0.0; size + 1]; // potential for cols
    let mut p = vec![0usize; size + 1]; // col assignment: p[j] = row assigned to col j
    let mut way = vec![0usize; size + 1];

    for i in 1..=size {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![f64::MAX; size + 1];
        let mut used = vec![false; size + 1];

        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = f64::MAX;
            let mut j1 = 0usize;

            for j in 1..=size {
                if !used[j] {
                    let cur = c[i0 - 1][j - 1] - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }

            for j in 0..=size {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }

            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }

        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    // Extract real assignments (skip padding)
    let mut result = Vec::new();
    for j in 1..=size {
        if p[j] > 0 && p[j] <= n && j <= m {
            result.push((p[j] - 1, j - 1));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_strings() {
        assert_eq!(normalized_edit_distance("hello", "hello"), 0.0);
    }

    #[test]
    fn test_completely_different() {
        assert_eq!(normalized_edit_distance("abc", "xyz"), 1.0);
    }

    #[test]
    fn test_empty_strings() {
        assert_eq!(normalized_edit_distance("", ""), 0.0);
    }

    #[test]
    fn test_one_empty() {
        assert_eq!(normalized_edit_distance("hello", ""), 1.0);
        assert_eq!(normalized_edit_distance("", "hello"), 1.0);
    }

    #[test]
    fn test_partial_match() {
        let ned = normalized_edit_distance("kitten", "sitting");
        // edit distance is 3, max len is 7
        assert!((ned - 3.0 / 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_omnidocbench_score() {
        assert_eq!(omnidocbench_text_score("hello", "hello"), 100.0);
        assert_eq!(omnidocbench_text_score("abc", "xyz"), 0.0);
    }

    #[test]
    fn test_latex_normalization() {
        let ref_text = r"$\mathbf{E}\mathbf{u}_{1-x}$";
        let normalized = normalize_for_ned(ref_text);
        // Case preserved (official v1.5), punctuation stripped
        assert_eq!(normalized, "Eu1x");
    }

    #[test]
    fn test_latex_frac() {
        let ref_text = r"\frac{a}{b}";
        let normalized = normalize_for_ned(ref_text);
        assert_eq!(normalized, "ab");
    }

    #[test]
    fn test_greek_unicode_normalization() {
        // \alpha should map to Unicode α (1 char), not "alpha" (5 chars)
        let ref_text = r"$\alpha + \beta$";
        let normalized = normalize_for_ned(ref_text);
        assert_eq!(normalized, "αβ");

        // Unicode Greek should pass through unchanged
        let hyp_text = "α + β";
        let normalized = normalize_for_ned(hyp_text);
        assert_eq!(normalized, "αβ");

        // Both forms should produce identical output
        assert_eq!(normalize_for_ned(r"$\alpha$"), normalize_for_ned("α"));
    }

    #[test]
    fn test_normalize_strips_formatting() {
        let md = "## Hello\n\n**world** test";
        let norm = normalize_for_ned(md);
        // Spaces stripped, case preserved (official v1.5 is case-sensitive)
        assert_eq!(norm, "Helloworldtest");
    }

    #[test]
    fn test_block_merge_scoring() {
        // Ref has 1 block, hyp has 2 (split by layout detector).
        // Old strict Hungarian would penalize heavily; quick_match merges them.
        let ref_blocks = vec!["Hello world this is a test".to_string()];
        let hyp_blocks = vec!["Hello world".to_string(), "this is a test".to_string()];
        let score = omnidocbench_text_score_blocks(&ref_blocks, &hyp_blocks);
        // After merging, should be a near-perfect match
        assert!(score > 90.0, "Merged blocks should score >90, got {}", score);
    }

    #[test]
    fn test_block_perfect_match() {
        let ref_blocks = vec!["hello world".to_string(), "foo bar".to_string()];
        let hyp_blocks = vec!["hello world".to_string(), "foo bar".to_string()];
        let score = omnidocbench_text_score_blocks(&ref_blocks, &hyp_blocks);
        assert_eq!(score, 100.0);
    }

    #[test]
    fn test_block_finer_granularity() {
        // PPT-like: ref has 2 blocks, hyp has 4 (each split in half)
        let ref_blocks = vec![
            "First line second line".to_string(),
            "Third line fourth line".to_string(),
        ];
        let hyp_blocks = vec![
            "First line".to_string(),
            "second line".to_string(),
            "Third line".to_string(),
            "fourth line".to_string(),
        ];
        let score = omnidocbench_text_score_blocks(&ref_blocks, &hyp_blocks);
        assert!(score > 80.0, "Finer-grained hyp should still score >80, got {}", score);
    }

    #[test]
    fn test_block_substring_match() {
        // Under-segmentation: ref has many blocks, hyp has one large block.
        // Substring matching should find each ref block within the hyp block.
        let ref_blocks = vec![
            "The quick brown fox jumps".to_string(),
            "over the lazy dog today".to_string(),
        ];
        let hyp_blocks = vec![
            "The quick brown fox jumps over the lazy dog today and more extra text here".to_string(),
        ];
        let score = omnidocbench_text_score_blocks(&ref_blocks, &hyp_blocks);
        // Should find both ref blocks as substrings, scoring much better than block-level NED
        assert!(score > 40.0, "Substring match should score >40, got {}", score);
    }
}
