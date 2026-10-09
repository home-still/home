//! HTML → markdown for the HTML ingest path and for EPUB chapters.
//!
//! The markup is untrusted (provider downloads, EPUB archives), and cost
//! grows with how deeply it nests (html5ever's tree construction is
//! quadratic in the open-element depth: 100 000 nested `<div>` take 21 s and
//! nothing can interrupt the call) and with how many nodes it builds (a DOM
//! costs ~22x the input). The bounds therefore sit on what the parser
//! *actually builds*, not on a guess about what it will build: html5ever is
//! driven in small chunks through a [`BoundedSink`] that counts nodes and
//! measures the real depth of every node it attaches, and parsing stops
//! between chunks — inside the parse — the moment a bound is crossed or the
//! caller's cancellation flag is raised. The walk over the finished tree is
//! iterative, so no document can overflow a stack with it.

use crate::classify::{ConvertFailure, FailureCode};
use ego_tree::iter::Edge;
use ego_tree::{NodeId, Tree};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{expanded_name, local_name, namespace_url, ns, Attribute, ParseOpts, QualName};
use scraper::node::{Comment, Element, Text};
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Bounds on one HTML conversion. Exceeding any of them fails the document
/// (nothing is truncated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HtmlLimits {
    /// Largest markup accepted, in bytes.
    pub max_input_bytes: usize,
    /// Deepest element nesting the parsed tree may reach.
    pub max_nesting: usize,
    /// Most nodes the tree may hold. Every attribute is charged here too (as
    /// one node each): a document of 16 MiB of attributes costs far more
    /// memory than its node count suggests.
    pub max_nodes: usize,
    /// Most attributes on any one element.
    pub max_attributes_per_element: usize,
    /// Most attributes in the whole document.
    pub max_attributes: usize,
    /// Wall-clock budget for one conversion (one HTML source, or a whole
    /// EPUB), in seconds. Parsing stops itself when it expires.
    pub max_convert_secs: u64,
}

impl Default for HtmlLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_nesting: 512,
            max_nodes: 1_000_000,
            max_attributes_per_element: 1_024,
            max_attributes: 1_000_000,
            // Measured (`a_book_length_document_parses_well_inside_the_budget`):
            // 8 MiB of book-like HTML parses and converts in ~4 s on a DEBUG
            // build (release is several times faster), so even the 16 MiB
            // input cap finishes in ~8 s there. 60 s leaves headroom for a
            // loaded host and is the real bound on costs no counter can see
            // (a tag with k attributes costs O(k^2) inside the tokenizer).
            max_convert_secs: 60,
        }
    }
}

impl HtmlLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_input_bytes == 0
            || self.max_nesting == 0
            || self.max_nodes == 0
            || self.max_attributes_per_element == 0
            || self.max_attributes == 0
            || self.max_convert_secs == 0
        {
            anyhow::bail!("html limits must all be at least 1: {self:?}");
        }
        Ok(())
    }
}

/// Markup fed to the parser per step; the check between steps is where a
/// bound or a cancellation stops the parse.
const CHUNK_BYTES: usize = 4096;

#[derive(Clone, Copy)]
enum Stop {
    Depth,
    Nodes,
    Attributes,
}

/// State shared between the sink (inside the parser) and the driver loop.
struct Shared {
    stop: Cell<Option<Stop>>,
    nodes: Cell<usize>,
    attributes: Cell<usize>,
}

/// scraper's tree sink, rebuilt over a tree this module can measure while it
/// grows. Dropped as unread: doctypes, comment text, processing instructions
/// and the attributes of a repeated `<html>`/`<body>`.
struct BoundedSink {
    tree: RefCell<Tree<Node>>,
    quirks: Cell<QuirksMode>,
    max_nesting: usize,
    max_nodes: usize,
    max_attributes_per_element: usize,
    max_attributes: usize,
    shared: Rc<Shared>,
}

impl BoundedSink {
    fn count_attributes(&self, count: usize) {
        let total = self.shared.attributes.get().saturating_add(count);
        self.shared.attributes.set(total);
        if (count > self.max_attributes_per_element || total > self.max_attributes)
            && self.shared.stop.get().is_none()
        {
            self.shared.stop.set(Some(Stop::Attributes));
        }
        // Charged to the node budget as well, one node per attribute.
        let n = self.shared.nodes.get().saturating_add(count);
        self.shared.nodes.set(n);
        if n > self.max_nodes && self.shared.stop.get().is_none() {
            self.shared.stop.set(Some(Stop::Nodes));
        }
    }

    fn count_node(&self) {
        let n = self.shared.nodes.get() + 1;
        self.shared.nodes.set(n);
        if n > self.max_nodes && self.shared.stop.get().is_none() {
            self.shared.stop.set(Some(Stop::Nodes));
        }
    }

    /// Note the depth reached by attaching something under `parent`. The
    /// walk up costs at most `max_nesting` steps: a longer chain is a stop.
    fn check_depth_under(&self, parent: NodeId) {
        if self.shared.stop.get().is_some() {
            return;
        }
        let tree = self.tree.borrow();
        let Some(node) = tree.get(parent) else { return };
        if node.ancestors().take(self.max_nesting).count() >= self.max_nesting {
            self.shared.stop.set(Some(Stop::Depth));
        }
    }

    fn empty_comment(&self) -> NodeId {
        self.count_node();
        self.tree
            .borrow_mut()
            .orphan(Node::Comment(Comment {
                comment: StrTendril::new(),
            }))
            .id()
    }
}

impl TreeSink for BoundedSink {
    type Output = Html;
    type Handle = NodeId;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Html {
        let mut html = Html::new_document();
        html.tree = self.tree.into_inner();
        html.quirks_mode = self.quirks.get();
        html
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks.set(mode);
    }

    fn get_document(&self) -> NodeId {
        self.tree.borrow().root().id()
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn elem_name<'a>(&'a self, target: &NodeId) -> Ref<'a, QualName> {
        Ref::map(self.tree.borrow(), |tree| {
            &tree
                .get(*target)
                .unwrap()
                .value()
                .as_element()
                .unwrap()
                .name
        })
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> NodeId {
        self.count_node();
        self.count_attributes(attrs.len());
        let template = name.expanded() == expanded_name!(html "template");
        let mut tree = self.tree.borrow_mut();
        let mut node = tree.orphan(Node::Element(Element::new(name, attrs)));
        if template {
            node.append(Node::Fragment);
        }
        node.id()
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        self.empty_comment()
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.empty_comment()
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.check_depth_under(*parent);
        let new_text_node = {
            let mut tree = self.tree.borrow_mut();
            let mut parent = tree.get_mut(*parent).unwrap();
            match child {
                NodeOrText::AppendNode(id) => {
                    parent.append_id(id);
                    false
                }
                NodeOrText::AppendText(text) => {
                    let merged = parent.last_child().is_some_and(|mut n| match n.value() {
                        Node::Text(t) => {
                            t.text.push_tendril(&text);
                            true
                        }
                        _ => false,
                    });
                    if !merged {
                        parent.append(Node::Text(Text { text }));
                    }
                    !merged
                }
            }
        };
        if new_text_node {
            self.count_node();
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let parent = self
            .tree
            .borrow()
            .get(*sibling)
            .and_then(|n| n.parent().map(|p| p.id()));
        if let Some(parent) = parent {
            self.check_depth_under(parent);
        }
        let new_text_node = {
            let mut tree = self.tree.borrow_mut();
            if let NodeOrText::AppendNode(id) = new_node {
                tree.get_mut(id).unwrap().detach();
            }
            let mut sibling = tree.get_mut(*sibling).unwrap();
            if sibling.parent().is_none() {
                false
            } else {
                match new_node {
                    NodeOrText::AppendNode(id) => {
                        sibling.insert_id_before(id);
                        false
                    }
                    NodeOrText::AppendText(text) => {
                        let merged = sibling.prev_sibling().is_some_and(|mut n| match n.value() {
                            Node::Text(t) => {
                                t.text.push_tendril(&text);
                                true
                            }
                            _ => false,
                        });
                        if !merged {
                            sibling.insert_before(Node::Text(Text { text }));
                        }
                        !merged
                    }
                }
            }
        };
        if new_text_node {
            self.count_node();
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self
            .tree
            .borrow()
            .get(*element)
            .is_some_and(|n| n.parent().is_some());
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.tree.borrow_mut().get_mut(*target).unwrap().detach();
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.check_depth_under(*new_parent);
        self.tree
            .borrow_mut()
            .get_mut(*new_parent)
            .unwrap()
            .reparent_from_id_append(*node);
    }

    fn add_attrs_if_missing(&self, _target: &NodeId, _attrs: Vec<Attribute>) {}

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.tree
            .borrow()
            .get(*target)
            .unwrap()
            .first_child()
            .unwrap()
            .id()
    }

    fn mark_script_already_started(&self, _node: &NodeId) {}
}

fn refuse(message: String) -> ConvertFailure {
    ConvertFailure::new(FailureCode::HtmlParseError, message)
}

/// Parse `html` under `limits`, in [`CHUNK_BYTES`] steps, stopping inside the
/// parse when a bound is crossed or `cancel` is raised.
fn parse_bounded(
    html: &str,
    limits: &HtmlLimits,
    cancel: &AtomicBool,
) -> Result<Html, ConvertFailure> {
    if html.len() > limits.max_input_bytes {
        return Err(refuse(format!(
            "HTML is {} bytes, over the limit of {}",
            html.len(),
            limits.max_input_bytes
        )));
    }
    let shared = Rc::new(Shared {
        stop: Cell::new(None),
        nodes: Cell::new(0),
        attributes: Cell::new(0),
    });
    let sink = BoundedSink {
        tree: RefCell::new(Tree::new(Node::Document)),
        quirks: Cell::new(QuirksMode::NoQuirks),
        max_nesting: limits.max_nesting,
        max_nodes: limits.max_nodes,
        max_attributes_per_element: limits.max_attributes_per_element,
        max_attributes: limits.max_attributes,
        shared: Rc::clone(&shared),
    };
    let mut parser = html5ever::parse_document(sink, ParseOpts::default());
    let mut rest = html;
    while !rest.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            return Err(refuse("HTML conversion was cancelled".to_string()));
        }
        let mut end = CHUNK_BYTES.min(rest.len());
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (chunk, tail) = rest.split_at(end);
        parser.process(StrTendril::from_slice(chunk));
        rest = tail;
        match shared.stop.get() {
            Some(Stop::Depth) => {
                return Err(refuse(format!(
                    "HTML elements nest more than {} levels deep",
                    limits.max_nesting
                )))
            }
            Some(Stop::Nodes) => {
                return Err(refuse(format!(
                    "HTML holds more than {} nodes",
                    limits.max_nodes
                )))
            }
            Some(Stop::Attributes) => {
                return Err(refuse(format!(
                    "HTML has more than {} attributes on one element or {} in total",
                    limits.max_attributes_per_element, limits.max_attributes
                )))
            }
            None => {}
        }
    }
    let doc = parser.finish();
    // The sink saw each node as it was attached, but html5ever also moves
    // whole subtrees (the adoption agency), which makes them deeper without
    // any attach point showing it. The bound is only real if it is measured
    // on the finished tree.
    let depth = max_element_depth(&doc);
    if depth > limits.max_nesting {
        return Err(refuse(format!(
            "HTML elements nest {depth} levels deep (limit {})",
            limits.max_nesting
        )));
    }
    Ok(doc)
}

/// Greatest number of elements on any root-to-leaf path, by one iterative
/// pass over the tree's traversal edges (no recursion, O(nodes)).
fn max_element_depth(doc: &Html) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    for edge in doc.tree.root().traverse() {
        match edge {
            Edge::Open(node) if node.value().is_element() => {
                depth += 1;
                max = max.max(depth);
            }
            Edge::Close(node) if node.value().is_element() => depth -= 1,
            _ => {}
        }
    }
    max
}

/// Convert an HTML academic paper to markdown.
/// Extracts the article body from PMC/PubMed-style HTML, preserving structure.
/// A document that exceeds `limits`, or a conversion `cancel` stops, is
/// refused with a typed [`FailureCode::HtmlParseError`].
pub fn convert_html_to_markdown(
    html: &str,
    limits: &HtmlLimits,
    cancel: &AtomicBool,
) -> Result<String, ConvertFailure> {
    let doc = parse_bounded(html, limits, cancel)?;
    Ok(markdown_of(&doc))
}

fn markdown_of(doc: &Html) -> String {
    let Some(root) = article_root(doc) else {
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
            cleaned.push_str(line.trim_end());
            cleaned.push('\n');
        }
    }
    cleaned.trim().to_string()
}

/// Append one text node's words. Whitespace at the node's edges survives as
/// a single space, so `Hello <b>world</b> again` does not become
/// `Hello**world**again`; a node that is only whitespace (the separator in
/// `<b>Hello</b> <i>world</i>`) adds one space unless `md` already ends in
/// whitespace, e.g. after a block or a table cell.
fn push_text(md: &mut String, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        if !text.is_empty() && !md.is_empty() && !md.ends_with(char::is_whitespace) {
            md.push(' ');
        }
        return;
    }
    if text.starts_with(char::is_whitespace) && !md.is_empty() && !md.ends_with(char::is_whitespace)
    {
        md.push(' ');
    }
    md.push_str(trimmed);
    if text.ends_with(char::is_whitespace) {
        md.push(' ');
    }
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
                    Node::Text(text) => push_text(md, text),
                    // `<template>` contents are inert: never part of the text.
                    Node::Fragment => dropped = Some(node.id()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        convert_html_to_markdown(html, &HtmlLimits::default(), &AtomicBool::new(false)).unwrap()
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

    #[test]
    fn the_article_body_is_found_by_id_and_class() {
        let html = r#"<html><body><div>menu</div><div id="article-body"><p>the paper</p></div></body></html>"#;
        let md = md(html);
        assert!(md.contains("the paper") && !md.contains("menu"), "{md}");
    }

    // ── the walker and the bounded parser against scraper's own ─────

    /// The walker as it was before it became iterative: the oracle the new
    /// one must match byte for byte.
    fn oracle_walk(element: &ElementRef, md: &mut String) {
        for child in element.children() {
            match child.value() {
                Node::Text(text) => push_text(md, text),
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
        for _ in 0..(1 + rng.below(3)) {
            match rng.below(5) {
                0 => out.push_str(rng.pick(&["alpha", "beta gamma", " delta ", "x < y", "a & b"])),
                1 if depth > 0 => {
                    let tag = *rng.pick(CONTENT_TAGS);
                    if tag == "br" {
                        out.push_str("<br>");
                    } else if tag == "table" {
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

    #[test]
    fn the_bounded_parser_and_iterative_walker_match_scrapers_on_well_formed_documents() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..300 {
            let mut body = String::new();
            well_formed(&mut rng, 6, &mut body);
            let html = format!("<html><body>{body}</body></html>");
            assert_eq!(md(&html), oracle_convert(&html), "case {case}: {html}");
        }
    }

    #[test]
    fn the_bounded_parser_builds_scrapers_tree_on_tag_soup() {
        // Same text out for arbitrary garbage: the sink port loses nothing
        // that reaches the markdown (generous limits, so nothing is refused).
        let limits = HtmlLimits {
            max_nesting: 100_000,
            ..HtmlLimits::default()
        };
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for case in 0..1_500 {
            let html = soup(&mut rng);
            let ours = convert_html_to_markdown(&html, &limits, &AtomicBool::new(false)).unwrap();
            assert_eq!(ours, oracle_convert(&html), "case {case}: {html}");
        }
    }

    // ── bounds, in a child process on a 2 MiB stack ─────────────────

    const CHILD_ENV: &str = "HS_SCRIBE_CHILD_HTML";

    /// Child entry. Each listed file is converted on a 2 MiB-stack thread
    /// under the default limits; the child reports `ok <bytes> <marker>` or
    /// `err <code>`, then its peak RSS.
    #[test]
    fn child_entry() {
        let Ok(spec) = std::env::var(CHILD_ENV) else {
            return;
        };
        for file in spec.split('\n') {
            let html = std::fs::read_to_string(file).unwrap();
            let outcome = std::thread::Builder::new()
                .stack_size(2 << 20)
                .spawn(move || {
                    convert_html_to_markdown(&html, &HtmlLimits::default(), &AtomicBool::new(false))
                })
                .unwrap()
                .join()
                .unwrap();
            match outcome {
                Ok(md) => println!("RESULT ok {} {}", md.len(), md.contains("marker")),
                Err(f) => println!("RESULT err {}", f.code().wire()),
            }
        }
        let peak_kb = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmHWM:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        println!("RESULT peak {peak_kb}");
        std::process::exit(0);
    }

    /// Convert each case in a child; it must exit normally within `limit`.
    /// Returns the per-case results and the child's peak RSS in kB.
    fn run_in_child(cases: &[String], limit: std::time::Duration) -> (Vec<String>, u64) {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (i, html) in cases.iter().enumerate() {
            let path = dir.path().join(format!("case-{i}.html"));
            std::fs::write(&path, html).unwrap();
            files.push(path.display().to_string());
        }
        let started = std::time::Instant::now();
        let child =
            crate::child_proc::run("html::tests::child_entry", CHILD_ENV, &files.join("\n"));
        assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
        let mut results = child.results();
        let peak = results.pop().unwrap();
        assert_eq!(results.len(), cases.len(), "{}", child.stdout);
        (results, peak.trim_start_matches("peak ").parse().unwrap())
    }

    fn nested(tag: &str, depth: usize) -> String {
        format!(
            "<html><body>{}marker{}</body></html>",
            format!("<{tag}>").repeat(depth),
            format!("</{tag}>").repeat(depth)
        )
    }

    /// The reviewer's unit that reaches real depth 3604 against a scan of 25.
    const MISNESTED_UNIT: &str = "<em></select><li/><body><tr>text <u></textarea><foreignObject/ <?><dt><big><templatex><font><s>/><math><button/></u>//<p><em><b><frameset><code/><strong><p><tt><b></body><span--!>>";

    #[test]
    fn every_hostile_nesting_is_refused_inside_the_parse_in_bounded_time() {
        let cases = vec![
            // Plain deep nesting.
            nested("div", 20_000),
            nested("span", 1_000_000),
            format!("<html><body>{}</body></html>", "<div><p>".repeat(100_000)),
            // A quote after `=` that html5ever reads as part of an attribute
            // name hid the rest of the document from the old byte scan.
            format!("<a =\"{}", "<div>".repeat(20_000)),
            // Mis-nested formatting markup the old scan under-counted 144x.
            MISNESTED_UNIT.repeat(1_500),
            // Legitimate depth still converts.
            nested("div", 300),
        ];
        let (results, _) = run_in_child(&cases, std::time::Duration::from_secs(60));
        for (i, result) in results[..5].iter().enumerate() {
            assert_eq!(result, "err html_parse_error", "case {i}: {result}");
        }
        assert!(
            results[5].starts_with("ok ") && results[5].ends_with("true"),
            "{}",
            results[5]
        );
    }

    #[test]
    fn the_dom_is_charged_to_a_budget_not_just_the_input() {
        // 8 MiB of `<i></i>` is under the input cap but builds ~1.2 million
        // nodes (179 MiB of DOM in the review's measurement).
        let doc = format!(
            "<html><body>{}</body></html>",
            "<i></i>".repeat(8 * 1024 * 1024 / 7)
        );
        let over_input_cap = "x".repeat(HtmlLimits::default().max_input_bytes + 1);
        let (results, peak_kb) =
            run_in_child(&[doc, over_input_cap], std::time::Duration::from_secs(60));
        assert_eq!(results[0], "err html_parse_error");
        assert_eq!(results[1], "err html_parse_error");
        assert!(peak_kb < 400 * 1024, "peak RSS {peak_kb} kB");
    }

    #[test]
    fn the_limits_are_the_configured_ones() {
        let flag = AtomicBool::new(false);
        let tight = HtmlLimits {
            max_input_bytes: 1_000,
            max_nesting: 5,
            max_nodes: 50,
            ..HtmlLimits::default()
        };
        let deep = nested("div", 10);
        assert!(convert_html_to_markdown(&deep, &tight, &flag).is_err());
        let wide = format!("<html><body>{}</body></html>", "<p>x</p>".repeat(100));
        assert!(convert_html_to_markdown(&wide, &tight, &flag).is_err());
        let big = format!("<html><body>{}</body></html>", "x".repeat(2_000));
        assert!(convert_html_to_markdown(&big, &tight, &flag).is_err());
        assert!(convert_html_to_markdown("<p>ok</p>", &tight, &flag).is_ok());
        assert!(HtmlLimits {
            max_nodes: 0,
            ..HtmlLimits::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn raising_the_cancellation_flag_stops_the_parse_itself() {
        let doc = format!("<html><body>{}</body></html>", "<p>x</p>".repeat(1_500_000));
        let limits = HtmlLimits {
            max_nodes: usize::MAX,
            ..HtmlLimits::default()
        };
        // Already raised: not even the first chunk is parsed.
        let raised = AtomicBool::new(true);
        let err = convert_html_to_markdown(&doc, &limits, &raised).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        // Raised mid-parse: the call returns long before it would have finished.
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let full = {
            let started = std::time::Instant::now();
            convert_html_to_markdown(&doc, &limits, &AtomicBool::new(false)).unwrap();
            started.elapsed()
        };
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(full / 10);
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let err = convert_html_to_markdown(&doc, &limits, &cancel).unwrap_err();
        raiser.join().unwrap();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(
            started.elapsed() < full / 2,
            "{:?} vs {:?}",
            started.elapsed(),
            full
        );
    }

    /// The reviewer's 235-byte unit: the adoption agency reparents subtrees,
    /// so the finished tree gets far deeper than any attach point showed.
    const ADOPTION_UNIT: &str = "</table><desc><object><script></font><nobr><template>x<i><dd></foreignObject></tr><math></h1>xxx</script></li><colgroup a=1></script></option><span><foreignObject></template><mi></i><button>x<svg></math></dd><li><math><svg>";

    #[test]
    fn the_depth_bound_holds_on_the_finished_tree_not_just_at_attach_points() {
        let doc = ADOPTION_UNIT.repeat(2_000);
        let tight = HtmlLimits {
            max_nesting: 64,
            ..HtmlLimits::default()
        };
        let err = convert_html_to_markdown(&doc, &tight, &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("levels deep"), "{err}");
        // The measure itself: more than the bound, as the review found.
        let tree = parse_bounded(
            &doc,
            &HtmlLimits {
                max_nesting: 100_000,
                ..HtmlLimits::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            max_element_depth(&tree) > 1_000,
            "{}",
            max_element_depth(&tree)
        );
        // And a document within the bound still converts.
        assert!(convert_html_to_markdown(
            &doc[..ADOPTION_UNIT.len() * 2],
            &HtmlLimits::default(),
            &AtomicBool::new(false)
        )
        .is_ok());
    }

    #[test]
    fn attributes_are_counted_and_charged_to_the_node_budget() {
        let flag = AtomicBool::new(false);
        let many = |k: usize| {
            let attrs: String = (0..k).map(|i| format!(" a{i}=1")).collect();
            format!("<html><body><div{attrs}>x</div></body></html>")
        };
        // Per element.
        let per_element = HtmlLimits {
            max_attributes_per_element: 100,
            ..HtmlLimits::default()
        };
        assert!(convert_html_to_markdown(&many(100), &per_element, &flag).is_ok());
        assert!(convert_html_to_markdown(&many(101), &per_element, &flag).is_err());
        // In total, across many elements each under the per-element cap.
        let total = HtmlLimits {
            max_attributes: 500,
            ..HtmlLimits::default()
        };
        let spread = format!(
            "<html><body>{}</body></html>",
            "<p a=1 b=2 c=3 d=4 e=5>x</p>".repeat(101)
        );
        assert!(convert_html_to_markdown(&spread, &total, &flag).is_err());
        // Charged to max_nodes: 60 attributes alone exceed a 50-node budget.
        let nodes = HtmlLimits {
            max_nodes: 50,
            ..HtmlLimits::default()
        };
        assert!(convert_html_to_markdown(&many(60), &nodes, &flag).is_err());
    }

    #[test]
    fn a_tag_with_a_hundred_thousand_attributes_is_stopped_by_the_wall_clock_not_hours() {
        // k distinct attributes on one tag cost O(k^2) inside the tokenizer
        // and no counter can see it before the tag ends (25k attributes took
        // 11 s, 200k took 720 s on a debug build). The cancellation flag is
        // the bound: raised after a short budget, the parse returns within
        // one 4 KiB chunk.
        let attrs: String = (0..400_000).map(|i| format!(" a{i}=1")).collect();
        let doc = format!("<html><body><div{attrs}>x</div></body></html>");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let raiser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let err = convert_html_to_markdown(&doc, &HtmlLimits::default(), &cancel).unwrap_err();
        raiser.join().unwrap();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_book_length_document_parses_well_inside_the_budget() {
        // ~8 MiB of ordinary prose with links, emphasis and a table every
        // so often: what a long book chapter looks like.
        let mut doc = String::from("<html><body><article>");
        while doc.len() < 8 * 1024 * 1024 {
            doc.push_str("<h2>Section</h2><p>Lorem ipsum <em>dolor</em> sit amet, <a href=\"https://example.org/a?b=c\">consectetur</a> adipiscing elit.</p><table><tr><td>1</td><td>2</td></tr></table>");
        }
        doc.push_str("</article></body></html>");
        let started = std::time::Instant::now();
        let md = convert_html_to_markdown(&doc, &HtmlLimits::default(), &AtomicBool::new(false))
            .unwrap();
        let took = started.elapsed();
        eprintln!(
            "BOOK-LENGTH: {} MiB parsed+converted in {took:?} (debug build)",
            doc.len() >> 20
        );
        assert!(md.contains("Lorem ipsum"));
        assert!(
            took.as_secs() < HtmlLimits::default().max_convert_secs / 2,
            "{took:?} against a {} s budget",
            HtmlLimits::default().max_convert_secs
        );
    }

    #[test]
    fn inline_elements_keep_the_spaces_around_them() {
        let md = md("<html><body><p>Hello <b>world</b> again, <i>x</i>y</p></body></html>");
        assert_eq!(md, "Hello **world** again, _x_y");
    }

    #[test]
    fn a_whitespace_only_node_still_separates_inline_elements() {
        assert_eq!(
            md("<html><body><p><b>Hello</b> <i>world</i></p></body></html>"),
            "**Hello** _world_"
        );
        assert_eq!(
            md("<html><body><p><span>John</span> <span>Smith</span></p></body></html>"),
            "John Smith"
        );
    }
}
