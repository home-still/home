use crate::models::layout::BBox;

/// Join per-page markdown strings into a single document.
pub fn join_pages(pages: &[String]) -> String {
    pages.join("\n\n---\n\n")
}

/// Assemble per-region OCR results into a single page's markdown.
///
/// Each region is formatted according to its PP-DocLayout-V3 class.
pub fn assemble_page_markdown(regions: &[(BBox, String)]) -> String {
    let mut parts = Vec::new();

    for (bbox, text) in regions {
        let text = text.trim();
        match bbox.class_name.as_str() {
            "doc_title" => {
                if !text.is_empty() {
                    parts.push(format!("# {}", text));
                }
            }
            "paragraph_title" => {
                if !text.is_empty() {
                    parts.push(format!("## {}", text));
                }
            }
            // Legacy YOLO title → H2
            "title" => {
                if !text.is_empty() {
                    parts.push(format!("## {}", text));
                }
            }
            "image" | "chart" | "seal" | "figure" => {
                parts.push("![Figure]".to_string());
            }
            "figure_title" | "figure_caption" | "table_caption" => {
                if !text.is_empty() {
                    parts.push(format!("*{}*", text));
                }
            }
            "display_formula" | "isolate_formula" => {
                if !text.is_empty() {
                    if text.starts_with("$$") {
                        parts.push(text.to_string());
                    } else {
                        parts.push(format!("$$\n{}\n$$", text));
                    }
                }
            }
            "inline_formula" => {
                if !text.is_empty() {
                    if text.starts_with('$') {
                        parts.push(text.to_string());
                    } else {
                        parts.push(format!("${}$", text));
                    }
                }
            }
            "algorithm" => {
                if !text.is_empty() {
                    parts.push(format!("```\n{}\n```", text));
                }
            }
            // Skip classes (defensive — should already be filtered)
            "header" | "footer" | "header_image" | "footer_image" | "number" | "formula_number" => {
                continue;
            }
            // text, abstract, content, reference, reference_content, footnote,
            // vision_footnote, aside_text, vertical_text, table, and anything else
            _ => {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(class_name: &str) -> BBox {
        BBox {
            x1: 0.0,
            y1: 0.0,
            x2: 100.0,
            y2: 100.0,
            confidence: 0.9,
            class_id: 0,
            class_name: class_name.to_string(),
            unique_id: 0,
            read_order: 0.0,
        }
    }

    #[test]
    fn test_doc_title_becomes_h1() {
        let regions = vec![(bbox("doc_title"), "Main Title".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "# Main Title");
    }

    #[test]
    fn empty_regions_assembles_to_empty_string() {
        // Phase A invariant: a page with zero layout regions flows through
        // execute_vlm_for_page with empty text_regions/table_regions and
        // produces empty markdown. assemble_page_markdown is the chokepoint
        // — if it ever non-trivially formats &[] the empty-page path breaks.
        let md = assemble_page_markdown(&[]);
        assert_eq!(md, "");
    }

    #[test]
    fn test_paragraph_title_becomes_h2() {
        let regions = vec![(bbox("paragraph_title"), "Section".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "## Section");
    }

    #[test]
    fn test_display_formula_wrapped() {
        let regions = vec![(bbox("display_formula"), "E = mc^2".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "$$\nE = mc^2\n$$");
    }

    #[test]
    fn test_display_formula_passthrough_if_already_wrapped() {
        let regions = vec![(bbox("display_formula"), "$$ E = mc^2 $$".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "$$ E = mc^2 $$");
    }

    #[test]
    fn test_inline_formula_wrapped() {
        let regions = vec![(bbox("inline_formula"), "x + y".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "$x + y$");
    }

    #[test]
    fn test_inline_formula_passthrough_if_already_wrapped() {
        let regions = vec![(bbox("inline_formula"), "$x + y$".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "$x + y$");
    }

    #[test]
    fn test_image_placeholder() {
        let regions = vec![(bbox("image"), String::new())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "![Figure]");
    }

    #[test]
    fn test_chart_placeholder() {
        let regions = vec![(bbox("chart"), String::new())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "![Figure]");
    }

    #[test]
    fn test_figure_title_italic() {
        let regions = vec![(bbox("figure_title"), "Figure 1: A chart".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "*Figure 1: A chart*");
    }

    #[test]
    fn test_algorithm_code_block() {
        let regions = vec![(bbox("algorithm"), "for i in range(n):".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "```\nfor i in range(n):\n```");
    }

    #[test]
    fn test_text_passthrough() {
        let regions = vec![(bbox("text"), "Hello world.".to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "Hello world.");
    }

    #[test]
    fn test_table_passthrough() {
        let table = "| A | B |\n|---|---|\n| 1 | 2 |";
        let regions = vec![(bbox("table"), table.to_string())];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, table);
    }

    #[test]
    fn test_skip_classes_omitted() {
        let regions = vec![
            (bbox("header"), "Page Header".to_string()),
            (bbox("text"), "Body text.".to_string()),
            (bbox("footer"), "Page 1".to_string()),
        ];
        let md = assemble_page_markdown(&regions);
        assert_eq!(md, "Body text.");
    }

    #[test]
    fn test_multi_region_assembly() {
        let regions = vec![
            (bbox("doc_title"), "Title Here".to_string()),
            (bbox("text"), "Some body text.".to_string()),
            (bbox("display_formula"), "x^2".to_string()),
        ];
        let md = assemble_page_markdown(&regions);
        assert!(md.contains("# Title Here"));
        assert!(md.contains("Some body text."));
        assert!(md.contains("$$\nx^2\n$$"));
    }
}
