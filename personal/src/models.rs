use serde::{Deserialize, Serialize};
use std::fmt;

/// The fixed taxonomy of categories. The LLM picks exactly one of these at
/// ingest time, or the user pins it via `--category`. New categories are added
/// here, never inferred at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Medical,
    Financial,
    Education,
    Legal,
    Employment,
    Tax,
    Insurance,
    Correspondence,
    Other,
}

impl Category {
    pub const ALL: &'static [Category] = &[
        Category::Medical,
        Category::Financial,
        Category::Education,
        Category::Legal,
        Category::Employment,
        Category::Tax,
        Category::Insurance,
        Category::Correspondence,
        Category::Other,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Medical => "medical",
            Category::Financial => "financial",
            Category::Education => "education",
            Category::Legal => "legal",
            Category::Employment => "employment",
            Category::Tax => "tax",
            Category::Insurance => "insurance",
            Category::Correspondence => "correspondence",
            Category::Other => "other",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Category {
    type Err = crate::error::PersonalError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for c in Self::ALL {
            if c.as_str().eq_ignore_ascii_case(s) {
                return Ok(*c);
            }
        }
        Err(crate::error::PersonalError::UnknownCategory(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_roundtrip_through_str() {
        for c in Category::ALL {
            let parsed: Category = c.as_str().parse().unwrap();
            assert_eq!(parsed, *c);
        }
    }

    #[test]
    fn category_parse_is_case_insensitive() {
        assert_eq!("Medical".parse::<Category>().unwrap(), Category::Medical);
        assert_eq!(
            "FINANCIAL".parse::<Category>().unwrap(),
            Category::Financial
        );
    }

    #[test]
    fn category_parse_rejects_unknown() {
        assert!("payroll".parse::<Category>().is_err());
        assert!("".parse::<Category>().is_err());
    }

    #[test]
    fn source_format_dispatch_covers_supported_extensions() {
        assert_eq!(SourceFormat::from_extension("pdf"), Some(SourceFormat::Pdf));
        assert_eq!(SourceFormat::from_extension("PDF"), Some(SourceFormat::Pdf));
        assert_eq!(
            SourceFormat::from_extension("epub"),
            Some(SourceFormat::Epub)
        );
        assert_eq!(
            SourceFormat::from_extension("docx"),
            Some(SourceFormat::Docx)
        );
        assert_eq!(
            SourceFormat::from_extension("md"),
            Some(SourceFormat::Markdown)
        );
        assert_eq!(
            SourceFormat::from_extension("markdown"),
            Some(SourceFormat::Markdown)
        );
        assert_eq!(SourceFormat::from_extension("txt"), Some(SourceFormat::Txt));
    }

    #[test]
    fn source_format_rejects_unknown_extensions() {
        assert_eq!(SourceFormat::from_extension("doc"), None);
        assert_eq!(SourceFormat::from_extension("rtf"), None);
        assert_eq!(SourceFormat::from_extension(""), None);
    }
}

/// File formats the personal pipeline accepts. Adding a new format requires
/// adding a converter under `personal::converters` and updating the dispatch
/// in `converters::mod::dispatch_by_extension`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceFormat {
    Pdf,
    Epub,
    Docx,
    Markdown,
    Txt,
}

impl SourceFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceFormat::Pdf => "pdf",
            SourceFormat::Epub => "epub",
            SourceFormat::Docx => "docx",
            SourceFormat::Markdown => "md",
            SourceFormat::Txt => "txt",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "pdf" => Some(SourceFormat::Pdf),
            "epub" => Some(SourceFormat::Epub),
            "docx" => Some(SourceFormat::Docx),
            "md" | "markdown" => Some(SourceFormat::Markdown),
            "txt" => Some(SourceFormat::Txt),
            _ => None,
        }
    }
}

impl fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
