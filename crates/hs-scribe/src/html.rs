use scraper::{ElementRef, Html, Node, Selector};

/// Convert an HTML academic paper to markdown.
/// Extracts the article body from PMC/PubMed-style HTML, preserving structure.
pub fn convert_html_to_markdown(html: &str) -> String {
    let doc = Html::parse_document(html);

    let body_selectors = ["article", "main", "#article-body", ".article-body", "body"];
    let mut root_html = None;
    for sel_str in &body_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                root_html = Some(el);
                break;
            }
        }
    }

    let root = match root_html {
        Some(el) => el,
        None => return doc.root_element().text().collect::<Vec<_>>().join(" "),
    };

    let mut md = String::new();
    walk_html_node(&root, &mut md);

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

fn walk_html_node(element: &ElementRef, md: &mut String) {
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
                    match tag {
                        "h1" => {
                            md.push_str("\n\n# ");
                            walk_html_node(&child_ref, md);
                            md.push_str("\n\n");
                        }
                        "h2" => {
                            md.push_str("\n\n## ");
                            walk_html_node(&child_ref, md);
                            md.push_str("\n\n");
                        }
                        "h3" => {
                            md.push_str("\n\n### ");
                            walk_html_node(&child_ref, md);
                            md.push_str("\n\n");
                        }
                        "h4" | "h5" | "h6" => {
                            md.push_str("\n\n#### ");
                            walk_html_node(&child_ref, md);
                            md.push_str("\n\n");
                        }
                        "p" | "div" => {
                            md.push_str("\n\n");
                            walk_html_node(&child_ref, md);
                            md.push_str("\n\n");
                        }
                        "strong" | "b" => {
                            md.push_str("**");
                            walk_html_node(&child_ref, md);
                            md.push_str("**");
                        }
                        "em" | "i" => {
                            md.push('_');
                            walk_html_node(&child_ref, md);
                            md.push('_');
                        }
                        "ul" | "ol" => {
                            md.push('\n');
                            walk_html_node(&child_ref, md);
                            md.push('\n');
                        }
                        "li" => {
                            md.push_str("\n- ");
                            walk_html_node(&child_ref, md);
                        }
                        "br" => md.push('\n'),
                        "a" => {
                            walk_html_node(&child_ref, md);
                        }
                        "sup" => {
                            md.push_str("<sup>");
                            walk_html_node(&child_ref, md);
                            md.push_str("</sup>");
                        }
                        "sub" => {
                            md.push_str("<sub>");
                            walk_html_node(&child_ref, md);
                            md.push_str("</sub>");
                        }
                        "table" | "thead" | "tbody" | "tr" | "td" | "th" => {
                            walk_html_node(&child_ref, md);
                            if tag == "tr" {
                                md.push('\n');
                            } else if tag == "td" || tag == "th" {
                                md.push_str(" | ");
                            }
                        }
                        "script" | "style" | "nav" | "footer" | "header" | "aside" | "noscript"
                        | "link" | "meta" => {}
                        _ => walk_html_node(&child_ref, md),
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_article_html() {
        let html = r#"<html><body><article>
            <h1>Title</h1>
            <p>Hello <strong>world</strong></p>
        </article></body></html>"#;
        let md = convert_html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("**world**"));
    }

    #[test]
    fn fallback_to_body_text() {
        let html = "<html><body>plain text only</body></html>";
        let md = convert_html_to_markdown(html);
        assert!(md.contains("plain text only"));
    }

    #[test]
    fn strips_scripts_and_nav() {
        let html = r#"<html><body><article>
            <nav>Menu</nav>
            <script>alert('x')</script>
            <p>Content</p>
        </article></body></html>"#;
        let md = convert_html_to_markdown(html);
        assert!(!md.contains("Menu"));
        assert!(!md.contains("alert"));
        assert!(md.contains("Content"));
    }
}
