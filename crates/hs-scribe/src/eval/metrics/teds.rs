/// Tree Edit Distance-based Similarity (TEDS) for table evaluation.
/// Implements the official TEDS metric from IBM's PubTabNet paper (ECCV 2020).
/// Parses HTML tables into tree structures and computes tree edit distance
/// using the Zhang-Shasha algorithm with TEDS-specific cost functions.
/// Score in [0, 100].

// ── Tree representation ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct TreeNode {
    tag: String, // e.g. "table", "tr", "td", "th", "thead", "tbody"
    colspan: usize,
    rowspan: usize,
    content: String, // text content (only meaningful for td/th)
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            colspan: 1,
            rowspan: 1,
            content: String::new(),
            children: Vec::new(),
        }
    }

    /// Count total nodes in this subtree.
    fn node_count(&self) -> usize {
        1 + self.children.iter().map(|c| c.node_count()).sum::<usize>()
    }
}

// ── HTML → Tree parser ─────────────────────────────────────────────────

/// Parse an HTML table string into a tree structure.
/// Handles: <table>, <thead>, <tbody>, <tr>, <td>, <th> with colspan/rowspan.
fn parse_html_table(html: &str) -> Option<TreeNode> {
    let mut root = TreeNode::new("table");
    let tokens = tokenize_html(html);

    // State machine parser
    let mut stack: Vec<TreeNode> = vec![TreeNode::new("table")];
    let mut in_cell = false;
    let mut cell_text = String::new();

    for token in &tokens {
        match token {
            HtmlToken::OpenTag(tag, attrs) => {
                let tag_lower = tag.to_lowercase();
                match tag_lower.as_str() {
                    "table" => {} // already have root
                    "thead" | "tbody" => {
                        stack.push(TreeNode::new(&tag_lower));
                    }
                    "tr" => {
                        stack.push(TreeNode::new("tr"));
                    }
                    "td" | "th" => {
                        let mut node = TreeNode::new(&tag_lower);
                        node.colspan = parse_span_attr(attrs, "colspan");
                        node.rowspan = parse_span_attr(attrs, "rowspan");
                        in_cell = true;
                        cell_text.clear();
                        stack.push(node);
                    }
                    "br" => {
                        // <br> inside a cell → treat as space separator
                        if in_cell {
                            cell_text.push(' ');
                        }
                    }
                    _ => {} // ignore other tags
                }
            }
            HtmlToken::CloseTag(tag) => {
                let tag_lower = tag.to_lowercase();
                match tag_lower.as_str() {
                    "td" | "th" => {
                        in_cell = false;
                        if let Some(mut node) = stack.pop() {
                            node.content = cell_text.trim().to_string();
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            }
                        }
                        cell_text.clear();
                    }
                    "tr" => {
                        if let Some(node) = stack.pop() {
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            }
                        }
                    }
                    "thead" | "tbody" => {
                        if let Some(node) = stack.pop() {
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            }
                        }
                    }
                    "table" => {} // handled at end
                    _ => {}
                }
            }
            HtmlToken::Text(text) => {
                if in_cell {
                    cell_text.push_str(&decode_html_entities(text));
                }
            }
        }
    }

    // If no thead/tbody wrapper, rows are direct children of table — that's fine.
    root = stack.into_iter().next().unwrap_or(root);

    // Flatten thead/tbody: promote their children (tr) to direct table children.
    // This ensures structural equivalence between tables with and without these wrappers,
    // since SLANet outputs thead/tbody but many references don't.
    let mut flattened_children = Vec::new();
    for child in root.children.drain(..) {
        match child.tag.as_str() {
            "thead" | "tbody" => {
                // Move tr children up to table level
                for grandchild in child.children {
                    flattened_children.push(grandchild);
                }
            }
            _ => flattened_children.push(child),
        }
    }
    root.children = flattened_children;

    // If table has no children (rows), wrap any direct td children in implicit tbody/tr
    if root.children.is_empty() {
        return None;
    }

    Some(root)
}

#[derive(Debug)]
enum HtmlToken {
    OpenTag(String, String), // (tag_name, full_attrs_string)
    CloseTag(String),
    Text(String),
}

fn tokenize_html(html: &str) -> Vec<HtmlToken> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = html.chars().collect();
    let mut i = 0;
    let mut text_buf = String::new();

    while i < chars.len() {
        if chars[i] == '<' {
            // Flush text buffer
            if !text_buf.is_empty() {
                tokens.push(HtmlToken::Text(std::mem::take(&mut text_buf)));
            }

            // Read tag
            i += 1;
            let mut tag_content = String::new();
            while i < chars.len() && chars[i] != '>' {
                tag_content.push(chars[i]);
                i += 1;
            }
            i += 1; // skip '>'

            let trimmed = tag_content.trim().to_string();
            if trimmed.starts_with('/') {
                let tag_name = trimmed[1..].trim().to_string();
                tokens.push(HtmlToken::CloseTag(tag_name));
            } else {
                // Split tag name from attributes
                let parts: Vec<&str> = trimmed.splitn(2, |c: char| c.is_whitespace()).collect();
                let tag_name = parts[0].trim_end_matches('/').to_string();
                let attrs = if parts.len() > 1 {
                    parts[1].to_string()
                } else {
                    String::new()
                };
                tokens.push(HtmlToken::OpenTag(tag_name, attrs));
            }
        } else {
            text_buf.push(chars[i]);
            i += 1;
        }
    }

    if !text_buf.is_empty() {
        tokens.push(HtmlToken::Text(text_buf));
    }

    tokens
}

fn parse_span_attr(attrs: &str, name: &str) -> usize {
    let lower = attrs.to_lowercase();
    if let Some(pos) = lower.find(name) {
        let rest = &lower[pos + name.len()..];
        let rest = rest.trim_start_matches(|c: char| c == '=' || c == ' ' || c == '"' || c == '\'');
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        num.parse().unwrap_or(1).max(1)
    } else {
        1
    }
}

// ── Zhang-Shasha Tree Edit Distance ────────────────────────────────────

/// Flatten tree into post-order indexed representation for Zhang-Shasha.
struct IndexedTree {
    nodes: Vec<IndexedNode>,
    // For each node i, leftmost_leaf[i] is the index of its leftmost leaf descendant
    leftmost_leaf: Vec<usize>,
    // Key roots for the algorithm
    key_roots: Vec<usize>,
}

struct IndexedNode {
    tag: String,
    colspan: usize,
    rowspan: usize,
    content: String,
}

impl IndexedTree {
    fn from_tree(root: &TreeNode) -> Self {
        let mut nodes = Vec::new();
        let mut leftmost_leaf = Vec::new();

        // Post-order traversal
        fn traverse(
            node: &TreeNode,
            nodes: &mut Vec<IndexedNode>,
            leftmost: &mut Vec<usize>,
        ) -> usize {
            let first_leaf = if node.children.is_empty() {
                nodes.len() // this node will be its own leftmost leaf
            } else {
                let mut fl = usize::MAX;
                for child in &node.children {
                    let child_fl = traverse(child, nodes, leftmost);
                    if fl == usize::MAX {
                        fl = child_fl;
                    }
                }
                fl
            };

            let _idx = nodes.len();
            nodes.push(IndexedNode {
                tag: node.tag.clone(),
                colspan: node.colspan,
                rowspan: node.rowspan,
                content: node.content.clone(),
            });
            leftmost.push(first_leaf);
            first_leaf
        }

        traverse(root, &mut nodes, &mut leftmost_leaf);

        // Compute key roots: nodes where leftmost_leaf differs from parent's
        let n = nodes.len();
        let mut key_roots = Vec::new();
        let mut visited = vec![false; n];

        for i in (0..n).rev() {
            let l = leftmost_leaf[i];
            if !visited[l] {
                key_roots.push(i);
                visited[l] = true;
            }
        }
        key_roots.sort();

        IndexedTree {
            nodes,
            leftmost_leaf,
            key_roots,
        }
    }
}

/// TEDS cost function for node comparison.
/// - Insert/delete any node: cost = 1
/// - Rename two nodes:
///   - If tags differ: cost = max_cost (effectively infinity → forces insert+delete)
///   - If both are td/th with different colspan/rowspan: cost = 1
///   - If both are td/th with same spans: cost = normalized edit distance on content
///   - Otherwise (structural nodes with same tag): cost = 0
fn rename_cost(a: &IndexedNode, b: &IndexedNode) -> f64 {
    if a.tag != b.tag {
        return 1.0; // Different tags → max cost
    }

    // Same tag
    match a.tag.as_str() {
        "td" | "th" => {
            // Check span attributes
            if a.colspan != b.colspan || a.rowspan != b.rowspan {
                return 1.0;
            }
            // Same spans → compare content via normalized edit distance.
            // Normalize cell content to strip LaTeX markup and Unicode variants,
            // since our OCR outputs plain text but references contain inline LaTeX
            // like `$\text{COD}_{cr}$` which our pipeline can't reproduce.
            let a_norm = super::edit_distance::normalize_for_ned(&a.content);
            let b_norm = super::edit_distance::normalize_for_ned(&b.content);
            if a_norm.is_empty() && b_norm.is_empty() {
                return 0.0;
            }
            let max_len = a_norm.len().max(b_norm.len());
            if max_len == 0 {
                return 0.0;
            }
            edit_distance(&a_norm, &b_norm) as f64 / max_len as f64
        }
        _ => 0.0, // Structural nodes with same tag → free rename
    }
}

/// Zhang-Shasha tree edit distance.
fn tree_edit_distance(t1: &IndexedTree, t2: &IndexedTree) -> f64 {
    let n1 = t1.nodes.len();
    let n2 = t2.nodes.len();

    if n1 == 0 && n2 == 0 {
        return 0.0;
    }
    if n1 == 0 {
        return n2 as f64;
    }
    if n2 == 0 {
        return n1 as f64;
    }

    // td[i][j] = tree distance between subtree rooted at i and subtree rooted at j
    let mut td = vec![vec![0.0f64; n2 + 1]; n1 + 1];
    // fd = forest distance (temporary, reused per key root pair)

    for &kr1 in &t1.key_roots {
        for &kr2 in &t2.key_roots {
            let l1 = t1.leftmost_leaf[kr1];
            let l2 = t2.leftmost_leaf[kr2];

            let rows = kr1 - l1 + 2;
            let cols = kr2 - l2 + 2;
            let mut fd = vec![vec![0.0f64; cols]; rows];

            // Base cases
            fd[0][0] = 0.0;
            for i in 1..rows {
                fd[i][0] = fd[i - 1][0] + 1.0; // delete cost
            }
            for j in 1..cols {
                fd[0][j] = fd[0][j - 1] + 1.0; // insert cost
            }

            for i in 1..rows {
                for j in 1..cols {
                    let node_i = l1 + i - 1; // actual node index in t1
                    let node_j = l2 + j - 1; // actual node index in t2

                    let li = t1.leftmost_leaf[node_i];
                    let lj = t2.leftmost_leaf[node_j];

                    if li == l1 && lj == l2 {
                        // Both are in the same "leftmost path" → tree distance
                        let cost = rename_cost(&t1.nodes[node_i], &t2.nodes[node_j]);
                        fd[i][j] = (fd[i - 1][j] + 1.0) // delete
                            .min(fd[i][j - 1] + 1.0) // insert
                            .min(fd[i - 1][j - 1] + cost); // rename
                        td[node_i + 1][node_j + 1] = fd[i][j];
                    } else {
                        // Forest distance → use previously computed tree distances
                        let ti = if li >= l1 { li - l1 } else { 0 };
                        let tj = if lj >= l2 { lj - l2 } else { 0 };
                        fd[i][j] = (fd[i - 1][j] + 1.0)           // delete
                            .min(fd[i][j - 1] + 1.0)               // insert
                            .min(fd[ti][tj] + td[node_i + 1][node_j + 1]); // match subtrees
                    }
                }
            }
        }
    }

    td[n1][n2]
}

// ── Public API ─────────────────────────────────────────────────────────

/// Compute TEDS score between reference and hypothesis HTML tables.
/// When input contains multiple tables (joined with \n), splits them
/// and uses greedy matching to pair ref/hyp tables by best TEDS score.
/// Returns average score in [0, 100] where 100 = identical.
pub fn teds_score(reference_html: &str, hypothesis_html: &str) -> Option<f64> {
    let ref_tables = split_tables(reference_html);
    let hyp_tables = split_tables(hypothesis_html);

    if ref_tables.is_empty() {
        return None;
    }

    if hyp_tables.is_empty() {
        return Some(0.0);
    }

    // Single table fast path
    if ref_tables.len() == 1 && hyp_tables.len() == 1 {
        return teds_score_single(&ref_tables[0], &hyp_tables[0]);
    }

    // Multiple tables: compute pairwise TEDS, then greedy best-match
    let mut scores_matrix: Vec<Vec<f64>> = Vec::new();
    for ref_html in &ref_tables {
        let mut row = Vec::new();
        for hyp_html in &hyp_tables {
            let s = teds_score_single(ref_html, hyp_html).unwrap_or(0.0);
            row.push(s);
        }
        scores_matrix.push(row);
    }

    // Global best-first greedy matching: pick highest scoring (ref, hyp) pair,
    // mark both as used, repeat. This maximizes total matched score.
    let mut used_ref = vec![false; ref_tables.len()];
    let mut used_hyp = vec![false; hyp_tables.len()];
    let mut total_score = 0.0;
    let min_pairs = ref_tables.len().min(hyp_tables.len());

    for _ in 0..min_pairs {
        let mut best_score = -1.0;
        let mut best_i = 0;
        let mut best_j = 0;
        for (i, row) in scores_matrix.iter().enumerate() {
            if used_ref[i] {
                continue;
            }
            for (j, &s) in row.iter().enumerate() {
                if used_hyp[j] {
                    continue;
                }
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
    // Unmatched ref tables contribute 0

    Some(total_score / ref_tables.len() as f64)
}

/// Compute TEDS for a single table pair.
/// Normalizes `<th>` → `<td>` since SLANet-Plus doesn't output `<th>`.
fn teds_score_single(reference_html: &str, hypothesis_html: &str) -> Option<f64> {
    // Apply official OmniDocBench normalization first (strips attributes, formatting tags)
    let ref_clean = normalize_table_html(reference_html);
    let hyp_clean = normalize_table_html(hypothesis_html);
    // Normalize header cells to data cells for fair comparison.
    // Must use word-boundary-aware replacement to avoid corrupting <thead> → <tdead>.
    let ref_normalized = normalize_th_to_td(&ref_clean);
    let hyp_normalized = normalize_th_to_td(&hyp_clean);
    let ref_tree = parse_html_table(&ref_normalized)?;
    let hyp_tree = parse_html_table(&hyp_normalized);

    let hyp_tree = match hyp_tree {
        Some(t) => t,
        None => return Some(0.0),
    };

    let ref_count = ref_tree.node_count();
    let hyp_count = hyp_tree.node_count();

    if ref_count == 0 && hyp_count == 0 {
        return Some(100.0);
    }

    let t1 = IndexedTree::from_tree(&ref_tree);
    let t2 = IndexedTree::from_tree(&hyp_tree);

    let dist = tree_edit_distance(&t1, &t2);
    let max_nodes = ref_count.max(hyp_count) as f64;

    let teds = (1.0 - dist / max_nodes).max(0.0) * 100.0;
    Some(teds)
}

/// Split concatenated table HTML into individual tables.
fn split_tables(html: &str) -> Vec<String> {
    let mut tables = Vec::new();
    let lower = html.to_lowercase();
    let mut search_from = 0;

    loop {
        let start = match lower[search_from..].find("<table") {
            Some(pos) => search_from + pos,
            None => break,
        };
        let end = match lower[start..].find("</table>") {
            Some(pos) => start + pos + "</table>".len(),
            None => break,
        };
        tables.push(html[start..end].to_string());
        search_from = end;
    }

    // Fallback: if no <table> tags found, try the whole string
    if tables.is_empty() && !html.trim().is_empty() {
        tables.push(html.to_string());
    }

    tables
}

/// Decode common HTML entities in text content.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x2019;", "\u{2019}")
        .replace("&nbsp;", " ")
}

/// Official OmniDocBench table HTML normalization.
/// Strips style/height/width/align/class attributes, removes formatting tags
/// (sub/sup/span/div/p/colgroup), and normalizes whitespace.
fn normalize_table_html(html: &str) -> String {
    let mut s = html.to_string();

    // Remove colgroup elements entirely
    s = remove_tag_pair(&s, "colgroup");

    // Remove formatting tags (keep their content): sub, sup, span, div, p, li, ul, ol, i, b, em, strong
    for tag in &[
        "sub", "sup", "span", "div", "p", "li", "ul", "ol", "i", "b", "em", "strong",
    ] {
        s = strip_tag_keep_content(&s, tag);
    }

    // Remove <html>, </html>, <body>, </body>, <tbody>, </tbody> wrappers
    for tag in &["html", "body", "tbody"] {
        s = strip_tag_keep_content(&s, tag);
    }

    // Remove style/height/width/align/class/border attributes from remaining tags
    s = strip_html_attributes(
        &s,
        &["style", "height", "width", "align", "class", "border"],
    );

    s
}

/// Remove a matched pair of tags and ALL content between them (e.g., `<colgroup>...</colgroup>`)
fn remove_tag_pair(html: &str, tag: &str) -> String {
    let lower = html.to_lowercase();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::with_capacity(html.len());
    let mut byte_pos = 0;
    let mut char_iter = html.char_indices().peekable();
    while byte_pos < html.len() {
        if lower[byte_pos..].starts_with(&open) {
            if let Some(close_pos) = lower[byte_pos..].find(&close) {
                byte_pos += close_pos + close.len();
                // Advance char_iter to match
                while char_iter.peek().is_some_and(|&(i, _)| i < byte_pos) {
                    char_iter.next();
                }
                continue;
            }
        }
        if let Some(&(_, ch)) = char_iter.peek() {
            result.push(ch);
            char_iter.next();
            byte_pos += ch.len_utf8();
        } else {
            break;
        }
    }
    result
}

/// Remove opening and closing tags but keep their text content (e.g., `<sub>x</sub>` → `x`)
fn strip_tag_keep_content(html: &str, tag: &str) -> String {
    let lower = html.to_lowercase();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::with_capacity(html.len());
    let mut byte_pos = 0;
    let mut char_iter = html.char_indices().peekable();
    while byte_pos < html.len() {
        // Check for opening tag (with possible attributes)
        if lower[byte_pos..].starts_with(&open) {
            let rest = &lower[byte_pos + open.len()..];
            if rest.starts_with('>') || rest.starts_with(' ') {
                // Skip to end of opening tag
                if let Some(gt) = html[byte_pos..].find('>') {
                    byte_pos += gt + 1;
                    while char_iter.peek().is_some_and(|&(i, _)| i < byte_pos) {
                        char_iter.next();
                    }
                    continue;
                }
            }
        }
        // Check for closing tag
        if lower[byte_pos..].starts_with(&close) {
            byte_pos += close.len();
            while char_iter.peek().is_some_and(|&(i, _)| i < byte_pos) {
                char_iter.next();
            }
            continue;
        }
        if let Some(&(_, ch)) = char_iter.peek() {
            result.push(ch);
            char_iter.next();
            byte_pos += ch.len_utf8();
        } else {
            break;
        }
    }
    result
}

/// Remove specified HTML attributes from all tags (e.g., `style="..."`, `class="..."`)
fn strip_html_attributes(html: &str, attrs: &[&str]) -> String {
    let mut result = html.to_string();
    for attr in attrs {
        loop {
            // Find attr=" case-insensitively by searching in lowercase copy
            let lower = result.to_lowercase();
            let pattern = format!("{}=\"", attr);
            let attr_start = match lower.find(&pattern) {
                Some(pos) => pos,
                None => break,
            };
            // Verify attr_start is a valid char boundary in result
            if !result.is_char_boundary(attr_start) {
                break;
            }
            // Strip one leading space if present
            let actual_start = if attr_start > 0
                && result.is_char_boundary(attr_start - 1)
                && result.as_bytes()[attr_start - 1] == b' '
            {
                attr_start - 1
            } else {
                attr_start
            };
            // Find closing quote
            let value_start = attr_start + pattern.len();
            if !result.is_char_boundary(value_start) {
                break;
            }
            if let Some(quote_end) = result[value_start..].find('"') {
                let end = value_start + quote_end + 1;
                if !result.is_char_boundary(end) {
                    break;
                }
                result = format!("{}{}", &result[..actual_start], &result[end..]);
            } else {
                break;
            }
        }
    }
    result
}

/// Replace `<th` with `<td` and `</th>` with `</td>` without corrupting `<thead>` or `</thead>`.
fn normalize_th_to_td(html: &str) -> String {
    // Use simple string replacement — safe for Unicode since the patterns are ASCII
    let result = html.replace("</th>", "</td>").replace("</TH>", "</td>");
    // Replace <th> and <th  (with attrs) but not <thead
    let mut out = String::with_capacity(result.len());
    let mut pos = 0;
    while pos < result.len() {
        if result[pos..].starts_with("<th>") {
            out.push_str("<td>");
            pos += 4;
        } else if result[pos..].starts_with("<th ") {
            out.push_str("<td ");
            pos += 4;
        } else if result[pos..].starts_with("<TH>") {
            out.push_str("<td>");
            pos += 4;
        } else if result[pos..].starts_with("<TH ") {
            out.push_str("<td ");
            pos += 4;
        } else {
            // Safe: advance by one character, not one byte
            let ch = result[pos..].chars().next().unwrap();
            out.push(ch);
            pos += ch.len_utf8();
        }
    }
    out
}

/// Character-level Levenshtein edit distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for j in 0..=n {
        prev[j] = j;
    }

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_tables() {
        let html = "<table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table>";
        let score = teds_score(html, html).unwrap();
        assert_eq!(score, 100.0);
    }

    #[test]
    fn test_completely_different() {
        let ref_html = "<table><tr><td>A</td></tr></table>";
        let hyp_html = "<table><tr><td>ZZZZZZZZZZZ</td></tr></table>";
        let score = teds_score(ref_html, hyp_html).unwrap();
        // Same structure (table > tr > td), only content differs
        // 3 nodes total, 1 node has content diff: TEDS = 1 - cost/3
        assert!(score > 0.0 && score < 100.0, "score was {}", score);
    }

    #[test]
    fn test_empty_hypothesis() {
        let ref_html = "<table><tr><td>A</td></tr></table>";
        let score = teds_score(ref_html, "").unwrap();
        assert_eq!(score, 0.0);
    }

    #[test]
    fn test_missing_row() {
        let ref_html = "<table><tr><td>A</td></tr><tr><td>B</td></tr></table>";
        let hyp_html = "<table><tr><td>A</td></tr></table>";
        let score = teds_score(ref_html, hyp_html).unwrap();
        // ref has 5 nodes (table, tr, td, tr, td), hyp has 3 nodes (table, tr, td)
        // Missing 1 tr + 1 td = cost 2, max_nodes = 5, TEDS = (1 - 2/5) * 100 = 60
        assert!((score - 60.0).abs() < 1.0, "score was {}", score);
    }

    #[test]
    fn test_colspan_difference() {
        let ref_html =
            r#"<table><tr><td colspan="2">Header</td></tr><tr><td>A</td><td>B</td></tr></table>"#;
        let hyp_html = "<table><tr><td>Header</td></tr><tr><td>A</td><td>B</td></tr></table>";
        let score = teds_score(ref_html, hyp_html).unwrap();
        // Same structure but different colspan on first td → cost 1 for that node
        assert!(score > 50.0 && score < 100.0, "score was {}", score);
    }

    #[test]
    fn test_parse_simple_table() {
        let html =
            "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Alice</td><td>30</td></tr></table>";
        let tree = parse_html_table(html).unwrap();
        assert_eq!(tree.tag, "table");
        assert_eq!(tree.children.len(), 2); // 2 rows
        assert_eq!(tree.children[0].children.len(), 2); // 2 cells in first row
        assert_eq!(tree.children[0].children[0].content, "Name");
    }

    #[test]
    fn test_parse_with_thead_tbody() {
        // thead/tbody should be flattened — rows promoted to table level
        let html =
            "<table><thead><tr><th>H</th></tr></thead><tbody><tr><td>D</td></tr></tbody></table>";
        let tree = parse_html_table(html).unwrap();
        assert_eq!(tree.tag, "table");
        assert_eq!(tree.children.len(), 2); // 2 rows (flattened from thead + tbody)
        assert_eq!(tree.children[0].tag, "tr");
        assert_eq!(tree.children[1].tag, "tr");
    }

    #[test]
    fn test_multi_table_split() {
        let tables =
            split_tables("<table><tr><td>A</td></tr></table>\n<table><tr><td>B</td></tr></table>");
        assert_eq!(tables.len(), 2);
        assert!(tables[0].contains("A"));
        assert!(tables[1].contains("B"));
    }

    #[test]
    fn test_multi_table_scoring() {
        let ref_html = "<table><tr><td>A</td></tr></table>\n<table><tr><td>B</td></tr></table>";
        let hyp_html = "<table><tr><td>A</td></tr></table>\n<table><tr><td>B</td></tr></table>";
        let score = teds_score(ref_html, hyp_html).unwrap();
        assert_eq!(score, 100.0);
    }

    #[test]
    fn test_thead_tbody_equivalence() {
        // Table with thead/tbody should score 100 against same table without wrappers
        let with_wrappers = "<table><thead><tr><td>H1</td><td>H2</td></tr></thead><tbody><tr><td>A</td><td>B</td></tr></tbody></table>";
        let without_wrappers =
            "<table><tr><td>H1</td><td>H2</td></tr><tr><td>A</td><td>B</td></tr></table>";
        let score = teds_score(with_wrappers, without_wrappers).unwrap();
        assert_eq!(
            score, 100.0,
            "thead/tbody flattening should give identical trees"
        );
    }

    #[test]
    fn test_multi_table_partial() {
        // 2 ref tables, only 1 hyp table matching the second
        let ref_html = "<table><tr><td>A</td></tr></table>\n<table><tr><td>B</td></tr></table>";
        let hyp_html = "<table><tr><td>B</td></tr></table>";
        let score = teds_score(ref_html, hyp_html).unwrap();
        // First ref table unmatched (0), second matches perfectly (100) → avg 50
        assert!((score - 50.0).abs() < 1.0, "score was {}", score);
    }

    #[test]
    fn test_normalize_strips_style_attrs() {
        // Table with style attributes should match same table without them
        let with_style = r#"<table border="1"><tr><td style="width: 100px">A</td></tr></table>"#;
        let without_style = "<table><tr><td>A</td></tr></table>";
        let score = teds_score(with_style, without_style).unwrap();
        assert_eq!(
            score, 100.0,
            "style attr stripping should give identical trees"
        );
    }

    #[test]
    fn test_normalize_strips_sub_sup() {
        // Cell content with <sub>/<sup> should match plain text version
        let with_sub = "<table><tr><td>H<sub>2</sub>O</td></tr></table>";
        let without_sub = "<table><tr><td>H2O</td></tr></table>";
        let score = teds_score(with_sub, without_sub).unwrap();
        assert_eq!(
            score, 100.0,
            "sub/sup stripping should give identical trees"
        );
    }

    #[test]
    fn test_normalize_handles_html_body_wrapper() {
        // Reference with <html><body> wrapper should match clean table
        let wrapped = "<html><body><table><tr><td>X</td></tr></table></body></html>";
        let clean = "<table><tr><td>X</td></tr></table>";
        let score = teds_score(wrapped, clean).unwrap();
        assert_eq!(
            score, 100.0,
            "html/body wrapper stripping should give identical trees"
        );
    }

    #[test]
    fn test_br_in_cell() {
        // <br> inside a cell should be treated as space
        let with_br = "<table><tr><td>line1<br>line2</td></tr></table>";
        let with_space = "<table><tr><td>line1 line2</td></tr></table>";
        let score = teds_score(with_br, with_space).unwrap();
        assert_eq!(score, 100.0, "br should be treated as space separator");
    }

    #[test]
    fn test_unicode_content_in_cells() {
        // Unicode characters like ζ (zeta, 2 bytes) must not cause panics
        let html_a = "<table><tr><td>ζ-potential</td><td>α value</td></tr></table>";
        let html_b = "<table><tr><td>ζ-potential</td><td>α value</td></tr></table>";
        let score = teds_score(html_a, html_b).unwrap();
        assert_eq!(
            score, 100.0,
            "identical tables with Unicode should score 100"
        );
    }

    #[test]
    fn test_normalize_with_unicode_content() {
        // normalize_table_html should handle sub/sup with Unicode content
        let html_with_sub = r#"<table><tr><td><sub>ζ</sub>-potential</td></tr></table>"#;
        let html_clean = "<table><tr><td>ζ-potential</td></tr></table>";
        let score = teds_score(html_with_sub, html_clean).unwrap();
        assert_eq!(score, 100.0, "sub stripping with Unicode should work");
    }
}
