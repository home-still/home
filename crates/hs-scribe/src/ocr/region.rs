/// Region type for task-specific VLM prompts.
///
/// Each variant maps to PP-DocLayout-V3 classes and carries the appropriate
/// prompt for GLM-OCR / compatible VLMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionType {
    /// text, paragraph_title, doc_title, abstract, content, reference,
    /// reference_content, footnote, vision_footnote, aside_text,
    /// vertical_text, figure_title, algorithm
    Text,
    /// table regions
    Table,
    /// display_formula
    Formula,
    /// inline_formula ($...$ wrapping)
    InlineFormula,
    /// figure, chart, seal — skip OCR, emit placeholder
    Figure,
    /// header, footer, header_image, footer_image, number, formula_number — omit entirely
    Skip,
    /// legacy full-page mode (no layout detection)
    FullPage,
}

impl RegionType {
    /// Return the task-specific prompt sent to the VLM.
    pub fn prompt(&self) -> &str {
        match self {
            RegionType::Text => "OCR:",
            RegionType::Table => "Table Recognition:",
            RegionType::Formula | RegionType::InlineFormula => "Formula Recognition:",
            RegionType::Figure => "",
            RegionType::Skip => "",
            RegionType::FullPage => "OCR:",
        }
    }

    /// Map a PP-DocLayout-V3 class name to a RegionType.
    pub fn from_class(class_name: &str) -> Self {
        match class_name {
            "text" | "paragraph_title" | "doc_title" | "abstract" | "content" | "reference"
            | "reference_content" | "footnote" | "vision_footnote" | "aside_text"
            | "vertical_text" | "figure_title" | "algorithm" => RegionType::Text,
            "table" => RegionType::Table,
            "display_formula" => RegionType::Formula,
            "inline_formula" => RegionType::InlineFormula,
            "image" | "chart" | "seal" => RegionType::Figure,
            "header" | "footer" | "header_image" | "footer_image" | "number" | "formula_number" => {
                RegionType::Skip
            }
            // Legacy DocLayout-YOLO class names (fallback)
            "title" | "plain text" | "figure_caption" | "table_caption" | "table_footnote"
            | "formula_caption" => RegionType::Text,
            "isolate_formula" => RegionType::Formula,
            "figure" => RegionType::Figure,
            "abandon" => RegionType::Skip,
            _ => RegionType::Text,
        }
    }
}
