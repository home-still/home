//! HTML → markdown for the HTML ingest path and for EPUB chapters.
//!
//! The markup is untrusted (provider downloads, EPUB archives), and two
//! things scale with how deeply it nests: the walk over the parsed tree and
//! html5ever's own tree construction. The walk is iterative, so no document
//! can overflow a stack with it. Tree construction is not recursive but it
//! is quadratic in the nesting depth (every start tag re-checks the open
//! elements: 100 000 nested `<div>` take ~20 s, a million take most of an
//! hour, and nothing can interrupt the parse), so [`convert_html_to_markdown`]
//! refuses a document whose elements nest more than [`MAX_HTML_NESTING`]
//! deep *before* any tree is built. Real documents nest a few dozen levels.

use crate::classify::{ConvertFailure, FailureCode};
use ego_tree::iter::Edge;
use scraper::{ElementRef, Html, Node, Selector};

/// Deepest element nesting the converter accepts. Parsing cost grows with
/// the square of the depth, and this bounds it to a few seconds on any
/// document the byte-size limits let through.
pub const MAX_HTML_NESTING: usize = 512;

/// Convert an HTML academic paper to markdown.
/// Extracts the article body from PMC/PubMed-style HTML, preserving structure.
/// A document nested deeper than [`MAX_HTML_NESTING`] is refused with a typed
/// [`FailureCode::HtmlParseError`].
pub fn convert_html_to_markdown(html: &str) -> Result<String, ConvertFailure> {
    let depth = nesting::max_open_elements(html.as_bytes(), MAX_HTML_NESTING);
    if depth > MAX_HTML_NESTING {
        return Err(ConvertFailure::new(
            FailureCode::HtmlParseError,
            format!("HTML elements nest more than {MAX_HTML_NESTING} levels deep"),
        ));
    }
    Ok(convert_unbounded(html))
}

/// Parse and walk with no nesting gate: [`convert_html_to_markdown`] after
/// its check.
fn convert_unbounded(html: &str) -> String {
    let doc = Html::parse_document(html);
    let Some(root) = article_root(&doc) else {
        return doc.root_element().text().collect::<Vec<_>>().join(" ");
    };
    let mut md = String::new();
    walk_html_node(&root, &mut md);
    clean_blank_lines(&md)
}

/// The element holding the article body: the first match of the selectors
/// below, most specific first.
fn article_root(doc: &Html) -> Option<ElementRef<'_>> {
    let body_selectors = ["article", "main", "#article-body", ".article-body", "body"];
    for sel_str in &body_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                return Some(el);
            }
        }
    }
    None
}

/// At most two consecutive blank lines, no leading or trailing whitespace.
fn clean_blank_lines(md: &str) -> String {
    let mut cleaned = String::new();
    let mut blank_count = 0u32;
    for line in md.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                cleaned.push('\n');
            }
        } else {
            blank_count = 0;
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    cleaned.trim().to_string()
}

/// Markdown written before and after the content of an element, or `None`
/// for an element that is transparent (its children are walked, nothing is
/// written for it).
fn wrap(tag: &str) -> Option<(&'static str, &'static str)> {
    Some(match tag {
        "h1" => ("\n\n# ", "\n\n"),
        "h2" => ("\n\n## ", "\n\n"),
        "h3" => ("\n\n### ", "\n\n"),
        "h4" | "h5" | "h6" => ("\n\n#### ", "\n\n"),
        "p" | "div" => ("\n\n", "\n\n"),
        "strong" | "b" => ("**", "**"),
        "em" | "i" => ("_", "_"),
        "ul" | "ol" => ("\n", "\n"),
        "li" => ("\n- ", ""),
        "br" => ("\n", ""),
        "sup" => ("<sup>", "</sup>"),
        "sub" => ("<sub>", "</sub>"),
        "tr" => ("", "\n"),
        "td" | "th" => ("", " | "),
        _ => return None,
    })
}

/// Elements whose whole subtree is dropped.
fn is_dropped(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "nav" | "footer" | "header" | "aside" | "noscript" | "link" | "meta"
    )
}

/// Walk the children of `element` in document order, appending markdown.
/// Iterative: the tree's own traversal events replace the call stack, so the
/// depth of the document costs no stack.
fn walk_html_node(element: &ElementRef, md: &mut String) {
    let root_id = element.id();
    // While set, everything inside the dropped element with this id is skipped.
    let mut dropped: Option<ego_tree::NodeId> = None;
    for edge in element.traverse() {
        match edge {
            Edge::Open(node) => {
                if node.id() == root_id || dropped.is_some() {
                    continue;
                }
                match node.value() {
                    Node::Text(text) => {
                        let t = text.trim();
                        if !t.is_empty() {
                            md.push_str(t);
                        }
                    }
                    Node::Element(el) => {
                        let tag = el.name();
                        if is_dropped(tag) {
                            dropped = Some(node.id());
                        } else if let Some((before, _)) = wrap(tag) {
                            md.push_str(before);
                        }
                    }
                    _ => {}
                }
            }
            Edge::Close(node) => {
                if node.id() == root_id {
                    continue;
                }
                if let Some(id) = dropped {
                    if node.id() == id {
                        dropped = None;
                    }
                    continue;
                }
                if let Node::Element(el) = node.value() {
                    if let Some((_, after)) = wrap(el.name()) {
                        md.push_str(after);
                    }
                }
            }
        }
    }
}

/// A byte scan of markup for the deepest stack of open elements html5ever
/// would build, run before anything is parsed.
///
/// It is not a second HTML parser: it tokenizes tags (comments, quoted
/// attribute values, raw-text elements) and keeps a stack of names, applying
/// the tree builder's rules for *popping* — implied end tags, scope-limited
/// end tags, the end of foreign content — only where they are exact, and
/// leaving an element open whenever unsure. Where the real parser's mode is
/// uncertain (an MathML `annotation-xml`, a `<font>` inside foreign content)
/// the scan assumes neither mode's shortcuts: no raw-text skipping, no
/// self-closing elements, no breakout. Its error is therefore meant to be an
/// over-estimate of the real depth, and sloppy-markup idioms (`<p>` after
/// `<p>`, `<li>` after `<li>`, unclosed table cells, repeated
/// `<html><body>` of concatenated documents) are handled exactly so that
/// real pages are not refused.
///
/// The one known source of under-counting is html5ever's re-opening of
/// formatting elements (`<b>`, `<i>`, `<font>`, ...) that a block closed
/// while they were open: the copies are made without a start tag. They are
/// bounded by the number of formatting start tags in the document, so the
/// real depth never exceeds the scan plus that count — at most about twice
/// the limit for any document the scan accepts. An under-estimate costs
/// only CPU (the walk is iterative and html5ever's construction is not
/// recursive), never a crash.
mod nesting {
    const SPECIAL: u32 = 1 << 0;
    const FORMATTING: u32 = 1 << 1;
    /// Stops the default scope (applet, caption, html, table, td, th,
    /// marquee, object, template and the foreign integration points).
    const BOUNDARY: u32 = 1 << 2;
    /// ol, ul: also stop the list-item scope.
    const LIST_BOUNDARY: u32 = 1 << 3;
    /// button: also stops the button scope.
    const BUTTON_BOUNDARY: u32 = 1 << 4;
    /// html, table, template: stop the table scope.
    const TABLE_BOUNDARY: u32 = 1 << 5;
    const VOID: u32 = 1 << 6;
    /// A start tag that closes an open `<p>`.
    const CLOSES_P: u32 = 1 << 9;
    const HEADING: u32 = 1 << 10;
    /// Table parts, by rank (td/th 3 > tr 2 > the rest 1).
    const CELL: u32 = 1 << 11;
    const ROW: u32 = 1 << 12;
    const SECTION: u32 = 1 << 13;
    /// Elements whose content is text, not markup, in HTML context.
    const RAW_TEXT: u32 = 1 << 14;
    const OPTION_LIKE: u32 = 1 << 15;
    const DD_DT: u32 = 1 << 16;
    const LI: u32 = 1 << 17;
    /// End tags that never pop (the insertion modes keep these open).
    const STAYS_OPEN: u32 = 1 << 18;
    /// A start tag ignored when this element is already open.
    const SINGLETON: u32 = 1 << 19;
    /// `form`: its end tag removes the form pointer, not what is above it.
    const FORM: u32 = 1 << 21;
    const SELECT: u32 = 1 << 20;

    /// Longest element name classified; a longer name is generic.
    const NAME_BUF: usize = 24;

    fn lowered<'b>(name: &[u8], buf: &'b mut [u8; NAME_BUF]) -> Option<&'b str> {
        if name.len() > NAME_BUF {
            return None;
        }
        for (b, c) in buf.iter_mut().zip(name) {
            *b = c.to_ascii_lowercase();
        }
        std::str::from_utf8(&buf[..name.len()]).ok()
    }

    fn class_of(name: &[u8]) -> u32 {
        let mut buf = [0u8; NAME_BUF];
        let Some(lower) = lowered(name, &mut buf) else {
            return 0;
        };
        match lower {
            "address" | "article" | "aside" | "blockquote" | "center" | "details" | "dialog"
            | "dir" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "header"
            | "hgroup" | "main" | "menu" | "nav" | "search" | "section" | "summary" | "div"
            | "pre" | "listing" | "p" => SPECIAL | CLOSES_P,
            "form" => SPECIAL | CLOSES_P | FORM,
            "xmp" => SPECIAL | CLOSES_P | RAW_TEXT,
            "ol" | "ul" => SPECIAL | CLOSES_P | LIST_BOUNDARY,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => SPECIAL | CLOSES_P | HEADING,
            "li" => SPECIAL | LI,
            "dd" | "dt" => SPECIAL | DD_DT,
            "button" => SPECIAL | BUTTON_BOUNDARY,
            "table" => SPECIAL | BOUNDARY | TABLE_BOUNDARY,
            "td" | "th" => SPECIAL | BOUNDARY | CELL,
            "tr" => SPECIAL | ROW,
            "tbody" | "thead" | "tfoot" | "colgroup" => SPECIAL | SECTION,
            "caption" => SPECIAL | BOUNDARY | SECTION,
            "template" => SPECIAL | BOUNDARY | TABLE_BOUNDARY,
            "html" => SPECIAL | BOUNDARY | TABLE_BOUNDARY | STAYS_OPEN | SINGLETON,
            "body" => SPECIAL | STAYS_OPEN | SINGLETON,
            "applet" | "marquee" | "object" => SPECIAL | BOUNDARY,
            "select" => SPECIAL | SELECT,
            "script" | "style" | "noscript" | "noframes" | "noembed" | "iframe" | "plaintext"
            | "textarea" | "title" => SPECIAL | RAW_TEXT,
            "optgroup" | "option" => SPECIAL | OPTION_LIKE,
            // Void elements, and the ones the tree builder ignores or replaces
            // wholesale outside their own insertion modes (`head` after the
            // head, `frameset` in a body): never pushed, so no end tag can
            // pop real elements through a stand-in for them.
            "area" | "base" | "basefont" | "bgsound" | "br" | "col" | "embed" | "hr" | "img"
            | "image" | "input" | "keygen" | "link" | "meta" | "param" | "source" | "track"
            | "wbr" | "head" | "frameset" | "frame" => VOID,
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small"
            | "strike" | "strong" | "tt" | "u" => FORMATTING,
            _ => 0,
        }
    }

    /// HTML elements that end foreign content when they start inside it.
    /// (`font` only does so with a `color`, `face` or `size` attribute: it is
    /// handled separately, as ambiguous.)
    fn breaks_out_of_foreign(name: &[u8]) -> bool {
        let mut buf = [0u8; NAME_BUF];
        lowered(name, &mut buf).is_some_and(|lower| {
            matches!(
                lower,
                "b" | "big"
                    | "blockquote"
                    | "body"
                    | "br"
                    | "center"
                    | "code"
                    | "dd"
                    | "div"
                    | "dl"
                    | "dt"
                    | "em"
                    | "embed"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "head"
                    | "hr"
                    | "i"
                    | "img"
                    | "li"
                    | "listing"
                    | "menu"
                    | "meta"
                    | "nobr"
                    | "ol"
                    | "p"
                    | "pre"
                    | "ruby"
                    | "s"
                    | "small"
                    | "span"
                    | "strong"
                    | "strike"
                    | "sub"
                    | "sup"
                    | "table"
                    | "tt"
                    | "u"
                    | "ul"
                    | "var"
            )
        })
    }

    /// SVG / MathML leaf elements that are routinely written `<x/>` and are
    /// really self-closing in foreign content. Nothing else is trusted to
    /// close itself: in an HTML context the slash is ignored.
    fn is_foreign_leaf(name: &[u8]) -> bool {
        let mut buf = [0u8; NAME_BUF];
        lowered(name, &mut buf).is_some_and(|lower| {
            matches!(
                lower,
                "path"
                    | "circle"
                    | "rect"
                    | "line"
                    | "polyline"
                    | "polygon"
                    | "ellipse"
                    | "use"
                    | "stop"
                    | "set"
                    | "animate"
                    | "animatemotion"
                    | "animatetransform"
                    | "mspace"
                    | "mprescripts"
                    | "none"
                    | "mglyph"
                    | "malignmark"
            ) || (lower.len() > 2 && lower.starts_with("fe"))
        })
    }

    /// How the real parser reads the content of the innermost open element
    /// that decides it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Html,
        Svg,
        Math,
        /// Could be foreign or HTML (MathML `annotation-xml`, SVG-looking
        /// `font`): assume neither.
        Ambiguous,
    }

    struct Open {
        start: usize,
        end: usize,
        class: u32,
    }

    struct Scan<'a> {
        html: &'a [u8],
        stack: Vec<Open>,
        max: usize,
        /// (index in `stack` of the element that set the mode, the mode of
        /// its content), innermost last.
        modes: Vec<(usize, Mode)>,
        /// Open HTML `select` elements: their content ignores most tags.
        selects: usize,
    }

    fn named(html: &[u8], o: &Open, name: &[u8]) -> bool {
        html[o.start..o.end].eq_ignore_ascii_case(name)
    }

    impl<'a> Scan<'a> {
        fn mode(&self) -> Mode {
            self.modes.last().map_or(Mode::Html, |m| m.1)
        }

        fn push(&mut self, start: usize, end: usize, class: u32, content_mode: Option<Mode>) {
            if let Some(mode) = content_mode {
                self.modes.push((self.stack.len(), mode));
            }
            if class & SELECT != 0 {
                self.selects += 1;
            }
            self.stack.push(Open { start, end, class });
            self.max = self.max.max(self.stack.len());
        }

        fn pop(&mut self) {
            if let Some(o) = self.stack.pop() {
                if o.class & SELECT != 0 {
                    self.selects = self.selects.saturating_sub(1);
                }
                while self.modes.last().is_some_and(|m| m.0 >= self.stack.len()) {
                    self.modes.pop();
                }
            }
        }

        /// Pop until `len` elements remain.
        fn truncate(&mut self, len: usize) {
            while self.stack.len() > len {
                self.pop();
            }
        }

        /// Index of the topmost element satisfying `is_target`, looking down
        /// from the top and giving up at the first element whose class meets
        /// `stops` (the target itself is tested first, as html5ever's scope
        /// checks do).
        fn find(&self, is_target: impl Fn(&Open) -> bool, stops: u32) -> Option<usize> {
            for (i, o) in self.stack.iter().enumerate().rev() {
                if is_target(o) {
                    return Some(i);
                }
                if o.class & stops != 0 {
                    return None;
                }
            }
            None
        }

        /// html5ever's "pop until X has been popped" after a scope check.
        fn close(&mut self, is_target: impl Fn(&Open) -> bool, stops: u32) {
            if let Some(i) = self.find(is_target, stops) {
                self.truncate(i);
            }
        }

        fn is_open(&self, name: &[u8]) -> bool {
            self.stack.iter().any(|o| named(self.html, o, name))
        }

        fn start_tag(&mut self, start: usize, end: usize, self_closing: bool) {
            let html = self.html;
            let name = &html[start..end];
            let class = class_of(name);
            match self.mode() {
                Mode::Html => self.start_html(start, end, name, class, self_closing),
                Mode::Svg | Mode::Math => {
                    if breaks_out_of_foreign(name) {
                        // The parser pops the foreign elements — all of the
                        // innermost foreign region — and reprocesses the tag as HTML.
                        if let Some(&(root, _)) = self.modes.last() {
                            self.truncate(root);
                        }
                        self.start_tag(start, end, self_closing);
                    } else if name.eq_ignore_ascii_case(b"font") {
                        // A breakout only with certain attributes: either
                        // reading is possible.
                        self.push(start, end, class, Some(Mode::Ambiguous));
                    } else if self_closing && is_foreign_leaf(name) {
                        // A genuine self-closing foreign element.
                    } else {
                        let content = self.foreign_content_mode(name);
                        // Integration points stop the default scope.
                        let class = if content.is_some() {
                            class | BOUNDARY
                        } else {
                            class
                        };
                        self.push(start, end, class, content);
                    }
                }
                Mode::Ambiguous => self.push(start, end, class, None),
            }
        }

        /// The mode inside a foreign element that changes it (an HTML
        /// integration point, MathML `annotation-xml`).
        fn foreign_content_mode(&self, name: &[u8]) -> Option<Mode> {
            let mut buf = [0u8; NAME_BUF];
            let lower = lowered(name, &mut buf)?;
            match (self.mode(), lower) {
                (Mode::Svg, "foreignobject" | "desc" | "title") => Some(Mode::Html),
                (Mode::Math, "mi" | "mo" | "mn" | "ms" | "mtext") => Some(Mode::Html),
                (Mode::Math, "annotation-xml") => Some(Mode::Ambiguous),
                _ => None,
            }
        }

        fn start_html(
            &mut self,
            start: usize,
            end: usize,
            name: &[u8],
            class: u32,
            self_closing: bool,
        ) {
            let html = self.html;
            if class & VOID != 0 {
                return;
            }
            // A second `<html>` / `<body>` / `<head>` merges into the open one.
            if class & SINGLETON != 0 && self.is_open(name) {
                return;
            }
            // Start tags that implicitly close what is open — exact rules only.
            if class & CLOSES_P != 0 {
                self.close(|o| named(html, o, b"p"), BOUNDARY | BUTTON_BOUNDARY);
            }
            if class & HEADING != 0 && self.stack.last().is_some_and(|t| t.class & HEADING != 0) {
                self.pop();
            }
            if class & (LI | DD_DT) != 0 {
                self.close_list_item(class & (LI | DD_DT));
            }
            if class & OPTION_LIKE != 0
                && self
                    .stack
                    .last()
                    .is_some_and(|t| t.class & OPTION_LIKE != 0 && named(html, t, b"option"))
            {
                self.pop();
            }
            // A table part outside a table (or template) is ignored by the
            // tree builder: it must not stand in the stack, or its end tag
            // would pop real elements through it.
            if class & (CELL | ROW | SECTION) != 0
                && !self
                    .stack
                    .iter()
                    .any(|o| named(html, o, b"table") || named(html, o, b"template"))
            {
                return;
            }
            self.close_table_parts(class);

            if name.eq_ignore_ascii_case(b"svg") || name.eq_ignore_ascii_case(b"math") {
                // Foreign elements honour `/>`.
                if self_closing {
                    return;
                }
                let kind = if name.eq_ignore_ascii_case(b"svg") {
                    Mode::Svg
                } else {
                    Mode::Math
                };
                self.push(start, end, class, Some(kind));
            } else {
                // HTML ignores the slash of a "self-closing" element.
                self.push(start, end, class, None);
            }
        }

        /// The tree builder's loop for `<li>`/`<dd>`/`<dt>`: close the open
        /// item of the same family, stopping at a special element that is
        /// not address/div/p.
        fn close_list_item(&mut self, family: u32) {
            let html = self.html;
            let mut found = None;
            for (i, o) in self.stack.iter().enumerate().rev() {
                if o.class & family != 0 {
                    found = Some(i);
                    break;
                }
                if o.class & SPECIAL != 0
                    && !(named(html, o, b"address")
                        || named(html, o, b"div")
                        || named(html, o, b"p"))
                {
                    break;
                }
            }
            if let Some(i) = found {
                self.truncate(i);
            }
        }

        /// A table part's start tag closes the parts it cannot sit in: only
        /// the consecutive ones on top of the stack.
        fn close_table_parts(&mut self, class: u32) {
            fn rank(c: u32) -> u8 {
                if c & CELL != 0 {
                    3
                } else if c & ROW != 0 {
                    2
                } else if c & SECTION != 0 {
                    1
                } else {
                    0
                }
            }
            let r = rank(class);
            if r == 0 {
                return;
            }
            while self
                .stack
                .last()
                .is_some_and(|top| rank(top.class) >= r && top.class & (CELL | ROW | SECTION) != 0)
            {
                self.pop();
            }
        }

        fn end_tag(&mut self, start: usize, end: usize) {
            let html = self.html;
            let name = &html[start..end];
            let class = class_of(name);
            let is_target = move |o: &Open| {
                if class & HEADING != 0 {
                    o.class & HEADING != 0
                } else {
                    named(html, o, name)
                }
            };
            match self.mode() {
                Mode::Svg | Mode::Math => {
                    // Foreign "any other end tag": pop to the nearest open
                    // element of that name within the foreign region.
                    let region = self.modes.last().map_or(0, |m| m.0);
                    if let Some(i) = self.find(is_target, 0) {
                        if i >= region {
                            self.truncate(i);
                        }
                    }
                    return;
                }
                Mode::Ambiguous => return,
                Mode::Html => {}
            }
            if class & (VOID | STAYS_OPEN) != 0 {
                return;
            }
            // Inside `select` the tree builder ignores every end tag but
            // these.
            if self.selects > 0
                && !(name.eq_ignore_ascii_case(b"select")
                    || name.eq_ignore_ascii_case(b"option")
                    || name.eq_ignore_ascii_case(b"optgroup")
                    || name.eq_ignore_ascii_case(b"template"))
            {
                return;
            }
            if class & FORM != 0 {
                // `</form>` removes the form pointer's element from the stack
                // without popping what is above it: close only when the
                // form is the current node.
                if self.stack.last().is_some_and(|t| t.class & FORM != 0) {
                    self.pop();
                }
                return;
            }
            if class & (CELL | ROW | SECTION) != 0 || name.eq_ignore_ascii_case(b"table") {
                // Table scope: html, table, template.
                self.close(is_target, TABLE_BOUNDARY);
            } else if class & FORMATTING != 0 {
                // The adoption agency algorithm: pop only across elements
                // that are not special; leave everything open otherwise.
                self.close(is_target, BOUNDARY | SPECIAL);
            } else if class & SPECIAL != 0 {
                let mut stops = BOUNDARY;
                if class & LI != 0 {
                    stops |= LIST_BOUNDARY;
                }
                if name.eq_ignore_ascii_case(b"p") {
                    stops |= BUTTON_BOUNDARY;
                }
                self.close(is_target, stops);
            } else {
                // Any other end tag: pop across non-special elements only.
                self.close(is_target, SPECIAL);
            }
        }
    }

    fn is_tag_name_end(b: u8) -> bool {
        b.is_ascii_whitespace() || b == b'/' || b == b'>'
    }

    fn name_end(html: &[u8], name_start: usize) -> usize {
        name_start
            + html[name_start..]
                .iter()
                .position(|&b| is_tag_name_end(b))
                .unwrap_or(html.len() - name_start)
    }

    /// Greatest number of simultaneously open elements, scanning `html` until
    /// it exceeds `limit` (then the value is above `limit`, which is all the
    /// caller needs).
    pub fn max_open_elements(html: &[u8], limit: usize) -> usize {
        let mut scan = Scan {
            html,
            stack: Vec::new(),
            max: 0,
            modes: Vec::new(),
            selects: 0,
        };
        let mut i = 0usize;
        while i < html.len() {
            let Some(rel) = html[i..].iter().position(|&b| b == b'<') else {
                break;
            };
            i += rel;
            match html.get(i + 1).copied() {
                Some(b'!') => {
                    if html[i + 1..].starts_with(b"!--") {
                        // A comment runs to `-->` / `--!>` (or an abrupt `>` /
                        // `->` right after the opener) or to the end of input.
                        let body_start = (i + 4).min(html.len());
                        match comment_end(&html[body_start..]) {
                            Some(at) => i = body_start + at,
                            None => break,
                        }
                    } else {
                        // DOCTYPE, `<![CDATA[` outside foreign content: up to `>`.
                        i = skip_past_gt(html, i + 2);
                    }
                }
                Some(b'?') => i = skip_past_gt(html, i + 2),
                Some(b'/') => {
                    let name_start = i + 2;
                    if html.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
                        let end = name_end(html, name_start);
                        let (after, _, _) = skip_tag_body(html, end);
                        scan.end_tag(name_start, end);
                        i = after;
                    } else {
                        i += 1;
                    }
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    let name_start = i + 1;
                    let end = name_end(html, name_start);
                    let (after, self_closing, complete) = skip_tag_body(html, end);
                    if !complete {
                        // A tag cut off by the end of the input is dropped.
                        break;
                    }
                    let name = &html[name_start..end];
                    // Raw text only where the real tokenizer switches: in HTML
                    // content, outside `select` (which ignores such tags).
                    let raw_text = class_of(name) & RAW_TEXT != 0
                        && scan.mode() == Mode::Html
                        && scan.selects == 0;
                    let before = scan.stack.len();
                    scan.start_tag(name_start, end, self_closing);
                    i = after;
                    if scan.max > limit {
                        return scan.max;
                    }
                    // A raw-text start tag is only believed once its end tag
                    // is found: otherwise the rest is read as markup, so a
                    // wrong belief can never hide anything.
                    if raw_text && scan.stack.len() > before {
                        if let Some(past_end_tag) = raw_text_end(html, i, name) {
                            scan.close(|o| named(html, o, name), 0);
                            i = past_end_tag;
                        }
                    }
                }
                _ => i += 1,
            }
        }
        scan.max
    }

    /// Offset just past the end of a comment whose body starts at `body[0]`.
    fn comment_end(body: &[u8]) -> Option<usize> {
        if body.starts_with(b">") {
            return Some(1);
        }
        if body.starts_with(b"->") {
            return Some(2);
        }
        let mut k = 0;
        while k + 1 < body.len() {
            if body[k] == b'-' && body[k + 1] == b'-' {
                match (body.get(k + 2), body.get(k + 3)) {
                    (Some(b'>'), _) => return Some(k + 3),
                    (Some(b'!'), Some(b'>')) => return Some(k + 4),
                    _ => {}
                }
            }
            k += 1;
        }
        None
    }

    /// Index just past the next `>` at or after `from` (or the end).
    fn skip_past_gt(html: &[u8], from: usize) -> usize {
        let from = from.min(html.len());
        match html[from..].iter().position(|&b| b == b'>') {
            Some(at) => from + at + 1,
            None => html.len(),
        }
    }

    /// Skip a tag's attributes, starting just after its name: returns (index
    /// past the closing `>`, whether the tag was written `.../>`, whether
    /// the closing `>` was found at all). Quoted attribute values may
    /// contain `>`.
    fn skip_tag_body(html: &[u8], from: usize) -> (usize, bool, bool) {
        let mut i = from;
        let mut slash = false;
        let mut expect_value = false;
        while i < html.len() {
            let b = html[i];
            match b {
                b'>' => return (i + 1, slash, true),
                b'/' => {
                    slash = !expect_value;
                    i += 1;
                }
                b'=' => {
                    expect_value = true;
                    slash = false;
                    i += 1;
                }
                b'"' | b'\'' if expect_value => {
                    match html[i + 1..].iter().position(|&c| c == b) {
                        Some(at) => i += at + 2,
                        None => return (html.len(), false, false),
                    }
                    expect_value = false;
                    slash = false;
                }
                c if c.is_ascii_whitespace() => i += 1,
                _ => {
                    if expect_value {
                        // Unquoted value: to whitespace or `>`.
                        while i < html.len() && !html[i].is_ascii_whitespace() && html[i] != b'>' {
                            i += 1;
                        }
                        expect_value = false;
                    } else {
                        i += 1;
                    }
                    slash = false;
                }
            }
        }
        (html.len(), false, false)
    }

    /// Offset just past the `</name ...>` that ends the raw-text element
    /// whose content starts at `from`; `None` when the end tag never comes.
    fn raw_text_end(html: &[u8], from: usize, name: &[u8]) -> Option<usize> {
        if name.eq_ignore_ascii_case(b"plaintext") {
            return None;
        }
        let mut i = from;
        loop {
            i += html.get(i..)?.iter().position(|&b| b == b'<')?;
            let after_slash = i + 2;
            if html.get(i + 1) == Some(&b'/')
                && html
                    .get(after_slash..after_slash + name.len())
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && html
                    .get(after_slash + name.len())
                    .is_some_and(|&b| is_tag_name_end(b))
            {
                let (past, _, complete) = skip_tag_body(html, after_slash + name.len());
                return Some(if complete { past } else { html.len() });
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        convert_html_to_markdown(html).unwrap()
    }

    #[test]
    fn basic_article_html() {
        let html = r#"<html><body><article>
            <h1>Title</h1>
            <p>Hello <strong>world</strong></p>
        </article></body></html>"#;
        let md = md(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("**world**"));
    }

    #[test]
    fn fallback_to_body_text() {
        let html = "<html><body>plain text only</body></html>";
        let md = md(html);
        assert!(md.contains("plain text only"));
    }

    #[test]
    fn strips_scripts_and_nav() {
        let html = r#"<html><body><article>
            <nav>Menu</nav>
            <script>alert('x')</script>
            <p>Content</p>
        </article></body></html>"#;
        let md = md(html);
        assert!(!md.contains("Menu"));
        assert!(!md.contains("alert"));
        assert!(md.contains("Content"));
    }

    // ── the iterative walker against the recursive original ─────────

    /// The walker as it was before it became iterative: the oracle the new
    /// one must match byte for byte.
    fn oracle_walk(element: &ElementRef, md: &mut String) {
        for child in element.children() {
            match child.value() {
                Node::Text(text) => {
                    let t = text.trim();
                    if !t.is_empty() {
                        md.push_str(t);
                    }
                }
                Node::Element(el) => {
                    let tag = el.name();
                    if let Some(child_ref) = ElementRef::wrap(child) {
                        if is_dropped(tag) {
                            continue;
                        }
                        match wrap(tag) {
                            Some((before, after)) => {
                                md.push_str(before);
                                oracle_walk(&child_ref, md);
                                md.push_str(after);
                            }
                            None => oracle_walk(&child_ref, md),
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn oracle_convert(html: &str) -> String {
        let doc = Html::parse_document(html);
        let Some(root) = article_root(&doc) else {
            return doc.root_element().text().collect::<Vec<_>>().join(" ");
        };
        let mut md = String::new();
        oracle_walk(&root, &mut md);
        clean_blank_lines(&md)
    }

    /// Small deterministic generator (xorshift): reproducible without a
    /// dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    const CONTENT_TAGS: &[&str] = &[
        "h1", "h2", "h3", "h5", "p", "div", "strong", "b", "em", "i", "ul", "ol", "li", "br", "a",
        "sup", "sub", "span", "section", "nav", "script", "style", "header", "footer", "aside",
        "noscript", "article", "table",
    ];

    /// A well-formed random fragment.
    fn well_formed(rng: &mut Rng, depth: usize, out: &mut String) {
        let breadth = 1 + rng.below(3);
        for _ in 0..breadth {
            match rng.below(5) {
                0 => out.push_str(rng.pick(&["alpha", "beta gamma", " delta ", "x < y", "a & b"])),
                1 if depth > 0 => {
                    let tag = *rng.pick(CONTENT_TAGS);
                    if tag == "br" {
                        out.push_str("<br>");
                    } else if tag == "table" {
                        // html5ever inserts the tbody a table row needs.
                        out.push_str("<table><tbody><tr><td>");
                        well_formed(rng, depth - 1, out);
                        out.push_str("</td></tr></tbody></table>");
                    } else {
                        out.push_str(&format!("<{tag}>"));
                        well_formed(rng, depth - 1, out);
                        out.push_str(&format!("</{tag}>"));
                    }
                }
                2 => out.push_str("<!-- <div> -->"),
                _ => out.push_str(rng.pick(&["word", "<i>t</i>", "<b>u</b>"])),
            }
        }
    }

    #[test]
    fn the_iterative_walker_matches_the_recursive_original() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..300 {
            let mut body = String::new();
            well_formed(&mut rng, 6, &mut body);
            let html = format!("<html><body>{body}</body></html>");
            assert_eq!(
                convert_unbounded(&html),
                oracle_convert(&html),
                "case {case}: {html}"
            );
        }
    }

    // ── hostile nesting, in a child process ─────────────────────────

    const CHILD_ENV: &str = "HS_SCRIBE_CHILD_HTML";

    /// Entry point of the child process. Each listed file is converted on a
    /// thread with tokio's default 2 MiB blocking stack — `ungated` skips the
    /// nesting bound, to prove the walk itself needs no stack — and the
    /// child reports `ok <bytes> <contains-marker>` or `err <code>`.
    #[test]
    fn child_entry() {
        let Ok(spec) = std::env::var(CHILD_ENV) else {
            return;
        };
        for line in spec.split('\n') {
            let (mode, file) = line.split_once(' ').unwrap();
            let html = std::fs::read_to_string(file).unwrap();
            let ungated = mode == "ungated";
            let outcome = std::thread::Builder::new()
                .stack_size(if ungated { 512 << 10 } else { 2 << 20 })
                .spawn(move || {
                    if ungated {
                        Ok(convert_unbounded(&html))
                    } else {
                        convert_html_to_markdown(&html)
                    }
                })
                .unwrap()
                .join()
                .unwrap();
            match outcome {
                Ok(md) => println!("RESULT ok {} {}", md.len(), md.contains("marker")),
                Err(f) => println!("RESULT err {}", f.code().wire()),
            }
        }
        std::process::exit(0);
    }

    fn run_in_child(cases: &[(&str, String)]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = Vec::new();
        for (i, (mode, html)) in cases.iter().enumerate() {
            let path = dir.path().join(format!("case-{i}.html"));
            std::fs::write(&path, html).unwrap();
            spec.push(format!("{mode} {}", path.display()));
        }
        let started = std::time::Instant::now();
        let child = crate::child_proc::run("html::tests::child_entry", CHILD_ENV, &spec.join("\n"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "took {:?}",
            started.elapsed()
        );
        let results = child.results();
        assert_eq!(results.len(), cases.len(), "{}", child.stdout);
        results
    }

    fn nested(tag: &str, depth: usize) -> String {
        format!(
            "<html><body>{}marker{}</body></html>",
            format!("<{tag}>").repeat(depth),
            format!("</{tag}>").repeat(depth)
        )
    }

    #[test]
    fn deeply_nested_documents_are_refused_before_a_tree_is_built() {
        // 20 000 nested <div>s aborted a 2 MiB-stack thread in the recursive
        // walker; a million of them would have parsed for most of an hour.
        let results = run_in_child(&[
            ("gated", nested("div", 20_000)),
            ("gated", nested("span", 1_000_000)),
            (
                "gated",
                format!("<html><body>{}</body></html>", "<div><p>".repeat(100_000)),
            ),
            ("gated", nested("div", MAX_HTML_NESTING - 5)),
        ]);
        assert_eq!(results[0], "err html_parse_error");
        assert_eq!(results[1], "err html_parse_error");
        assert_eq!(results[2], "err html_parse_error");
        assert!(
            results[3].starts_with("ok ") && results[3].ends_with("true"),
            "{}",
            results[3]
        );
    }

    #[test]
    fn the_walk_itself_needs_no_stack() {
        // The same 20 000-deep document, but past the nesting gate and on a
        // 512 KiB stack: the recursive walker overflowed 2 MiB at this depth.
        let results = run_in_child(&[("ungated", nested("div", 20_000))]);
        assert!(
            results[0].starts_with("ok ") && results[0].ends_with("true"),
            "{}",
            results[0]
        );
    }

    // ── the nesting scan ────────────────────────────────────────────

    fn depth(html: &str) -> usize {
        nesting::max_open_elements(html.as_bytes(), usize::MAX)
    }

    #[test]
    fn nesting_counts_open_elements() {
        assert_eq!(depth("<div><p><b>x</b></p></div>"), 3);
        assert_eq!(depth("<div></div><div></div>"), 1);
        assert_eq!(depth(&nested("div", 40)), 40 + 2); // + html, body
    }

    #[test]
    fn markup_that_is_not_an_element_does_not_count() {
        for html in [
            "<!-- <div><div><div> --><p>x</p>",
            "<!DOCTYPE html><br><br><hr><img src=a><input><meta><link>",
            r#"<a title="<div><div><div>" href='<span><span>'>x</a>"#,
            "<script>if (a<b) { x = '<div><div><div>'; }</script>",
            "<style>a > b { } /* <div><div> */</style>",
            "<textarea><div><div><div></textarea>",
            "<title><b><b><b></title>",
        ] {
            assert!(depth(html) <= 1, "{html}: {}", depth(html));
        }
    }

    #[test]
    fn unclosed_but_ordinary_markup_does_not_inflate_the_depth() {
        // Idioms every real-world page uses; each must not nest.
        let paragraphs = "<p>one<p>two<p>three<p>four".repeat(200);
        assert!(depth(&paragraphs) <= 2, "p: {}", depth(&paragraphs));
        let items = format!("<ul>{}</ul>", "<li>item".repeat(500));
        assert!(depth(&items) <= 3, "li: {}", depth(&items));
        let defs = format!("<dl>{}</dl>", "<dt>t<dd>d".repeat(500));
        assert!(depth(&defs) <= 3, "dl: {}", depth(&defs));
        let cells = format!("<table><tr>{}</table>", "<td>c".repeat(500));
        assert!(depth(&cells) <= 4, "td: {}", depth(&cells));
        let rows = format!("<table>{}</table>", "<tr><td>a<td>b".repeat(500));
        assert!(depth(&rows) <= 4, "tr: {}", depth(&rows));
        let options = format!("<select>{}</select>", "<option>o".repeat(500));
        assert!(depth(&options) <= 3, "option: {}", depth(&options));
        // A block end tag closes the unclosed inline elements inside it.
        let sloppy = "<div><span>text</div>".repeat(500);
        assert!(depth(&sloppy) <= 3, "div: {}", depth(&sloppy));
        let bold = "<p><b>text</p>".repeat(500);
        assert!(depth(&bold) <= 3, "b: {}", depth(&bold));
    }

    #[test]
    fn tricks_that_hide_nesting_from_a_naive_count_do_not_work() {
        let many = 2_000;
        for (name, html) in [
            // HTML ignores the slash of a "self-closing" non-void element.
            ("self-closing div", "<div/>".repeat(many)),
            // A stray end tag of another name does not pop anything.
            ("stray closers", "<div></span>".repeat(many)),
            // An end tag across a special element is ignored.
            ("span across div", "<span><div></span>".repeat(many)),
            // Slash-looking text inside an attribute value is not a self-close.
            ("slash in value", "<div a=b/>".repeat(many)),
            // Quotes that hide the closing `>`.
            ("quoted gt", "<div a=\">\">".repeat(many)),
            // Foreign content: an HTML breakout inside svg is an HTML element.
            ("svg breakout", format!("<svg>{}", "<div/>".repeat(many))),
            (
                "foreignObject",
                format!("<svg><foreignObject>{}", "<section/>".repeat(many)),
            ),
            // style/script inside svg is markup, not text.
            ("svg style", format!("<svg><style>{}", "<div>".repeat(many))),
        ] {
            assert!(
                depth(&html) >= many,
                "{name}: scan saw only {}",
                depth(&html)
            );
        }
    }

    // ── the scan against html5ever ──────────────────────────────────

    /// Most elements on any root-to-leaf path of the tree html5ever builds.
    fn real_depth(html: &str) -> usize {
        let doc = Html::parse_document(html);
        doc.tree
            .nodes()
            .filter(|n| n.value().is_element())
            .map(|n| n.ancestors().filter(|a| a.value().is_element()).count() + 1)
            .max()
            .unwrap_or(0)
    }

    const SOUP_TAGS: &[&str] = &[
        "div",
        "span",
        "p",
        "li",
        "ul",
        "ol",
        "b",
        "i",
        "a",
        "table",
        "tr",
        "td",
        "th",
        "tbody",
        "svg",
        "g",
        "math",
        "mi",
        "foreignObject",
        "style",
        "script",
        "textarea",
        "title",
        "select",
        "option",
        "form",
        "h1",
        "h2",
        "section",
        "br",
        "dd",
        "dt",
        "dl",
        "button",
        "template",
        "noscript",
        "iframe",
        "xmp",
        "font",
        "pre",
        "caption",
        "html",
        "body",
        "head",
    ];

    fn soup(rng: &mut Rng) -> String {
        let mut out = String::new();
        for _ in 0..(5 + rng.below(120)) {
            let tag = *rng.pick(SOUP_TAGS);
            match rng.below(10) {
                0..=3 => out.push_str(&format!("<{tag}>")),
                4 | 5 => out.push_str(&format!("</{tag}>")),
                6 => out.push_str(&format!("<{tag}/>")),
                7 => out.push_str(&format!("<{tag} a=\"x>y\" b='/'>")),
                8 => out.push_str(rng.pick(&["text", "<!-- c -->", "&amp;", "<p>", "</p>"])),
                _ => out.push_str(&format!("<{tag} k=v/>")),
            }
        }
        out
    }

    /// Start tags of formatting elements: what html5ever may re-open after
    /// a block closed them.
    fn formatting_start_tags(html: &str) -> usize {
        const NAMES: &[&str] = &[
            "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong",
            "tt", "u",
        ];
        let lower = html.to_ascii_lowercase();
        lower
            .match_indices('<')
            .filter(|(at, _)| {
                let rest = &lower[at + 1..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                NAMES.contains(&name.as_str())
            })
            .count()
    }

    /// The soundness property: html5ever's depth never exceeds the scan's
    /// plus the formatting-element copies it may re-open plus the html, head
    /// and body it adds around any fragment.
    fn undercounted(html: &str) -> bool {
        const IMPLICIT: usize = 4;
        depth(html) + formatting_start_tags(html) + IMPLICIT < real_depth(html)
    }

    #[test]
    fn the_scan_never_under_counts_what_html5ever_builds() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for _ in 0..6_000 {
            let html = soup(&mut rng);
            assert!(
                !undercounted(&html),
                "scan {} + {} formatting tags, but html5ever built {} deep: {html}",
                depth(&html),
                formatting_start_tags(&html),
                real_depth(&html)
            );
        }
    }

    #[test]
    fn the_scan_is_tight_on_well_formed_documents() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..500 {
            let mut body = String::new();
            well_formed(&mut rng, 8, &mut body);
            let html = format!("<html><body>{body}</body></html>");
            let (scan, real) = (depth(&html), real_depth(&html));
            assert!(scan <= real, "scan {scan} over html5ever's {real}: {html}");
            assert!(
                real <= scan + 1,
                "scan {scan} under html5ever's {real}: {html}"
            );
        }
    }
}
