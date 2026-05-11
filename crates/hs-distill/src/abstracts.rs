//! Coalesce + extract paper abstracts for the `paper_abstracts` Qdrant
//! collection.
//!
//! Three input sources, evaluated in order:
//! 1. **Local OpenAlex catalog** — `works.abstract_text` looked up by DOI.
//!    Plain reconstructed text (parser already inverted the
//!    `abstract_inverted_index` at ingest), highest quality.
//! 2. **VLM-converted markdown** — heuristic extraction of an `Abstract`
//!    section from the converted paper. Coverage is partial: scribe's
//!    `markdown_generator` emits "abstract"-classified regions as plain
//!    paragraphs, so `## Abstract` only appears when the PDF's layout
//!    triggered the `paragraph_title` classifier on the heading line.
//!    Empirical hit rate on the existing corpus: ~24% `## Abstract`,
//!    ~46% bare `Abstract`-on-its-own-line.
//! 3. **Title only** — last resort so every downloaded paper makes it into
//!    the abstracts index. Caller is responsible for falling through to
//!    this when neither (1) nor (2) yielded usable text.
//!
//! Coalesce is deterministic at index time (not a runtime fallback). The
//! chosen source is stamped into the Qdrant payload and the catalog so
//! downstream code can filter by provenance.

use serde::{Deserialize, Serialize};

/// Minimum non-whitespace length below which a candidate abstract is
/// treated as garbage and the coalesce falls through to the next source.
/// Picked to reject degenerate one-line strings ("Abstract not available",
/// "See PDF.") while still accepting genuinely-short abstracts from older
/// papers.
pub const MIN_ABSTRACT_CHARS: usize = 100;

/// Provenance of the abstract used for one paper's embedding. Serialized
/// form is what gets stamped into `CatalogEntry::abstract_embed.source`
/// and the Qdrant payload's `source` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AbstractSource {
    /// `works.abstract_text` from the local OpenAlex DuckDB.
    Openalex,
    /// Extracted from the converted markdown's Abstract section.
    Markdown,
    /// No usable abstract from either source — embed the title alone.
    TitleOnly,
}

impl AbstractSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AbstractSource::Openalex => "openalex",
            AbstractSource::Markdown => "markdown",
            AbstractSource::TitleOnly => "title_only",
        }
    }
}

/// Result of running the coalesce. `abstract_text` is `None` only when
/// `source == TitleOnly` (the caller embeds just the title in that case).
#[derive(Debug, Clone)]
pub struct CoalescedAbstract {
    pub source: AbstractSource,
    pub abstract_text: Option<String>,
}

impl CoalescedAbstract {
    pub fn abstract_chars(&self) -> u32 {
        self.abstract_text
            .as_deref()
            .map(|s| s.chars().count() as u32)
            .unwrap_or(0)
    }
}

/// Pick the best of (openalex catalog abstract, markdown-extracted
/// abstract, title-only). Each candidate must be `>= MIN_ABSTRACT_CHARS`
/// to be accepted; otherwise we fall through.
///
/// Inputs are pre-fetched so this function stays IO-free and testable.
/// The caller (in the `hs distill abstracts build` flow) does the DOI
/// lookup against the OpenAlex DuckDB and the markdown fetch via the
/// storage backend, then passes the two `Option<String>` candidates in.
pub fn coalesce_abstract(
    openalex_abstract: Option<String>,
    markdown_body: Option<&str>,
) -> CoalescedAbstract {
    if let Some(text) = openalex_abstract.filter(|s| s.trim().len() >= MIN_ABSTRACT_CHARS) {
        return CoalescedAbstract {
            source: AbstractSource::Openalex,
            abstract_text: Some(text.trim().to_string()),
        };
    }

    if let Some(body) = markdown_body {
        if let Some(extracted) =
            extract_markdown_abstract(body).filter(|s| s.len() >= MIN_ABSTRACT_CHARS)
        {
            return CoalescedAbstract {
                source: AbstractSource::Markdown,
                abstract_text: Some(extracted),
            };
        }
    }

    CoalescedAbstract {
        source: AbstractSource::TitleOnly,
        abstract_text: None,
    }
}

/// Build the embedding input string from a coalesce result. The title is
/// always included as a prefix — standard practice for paper-level
/// embeddings (SPECTER/SciNCL pretrain on `title [SEP] abstract`), and
/// also the only signal we have for the `TitleOnly` source.
pub fn build_embed_input(title: Option<&str>, coalesced: &CoalescedAbstract) -> String {
    let title = title.unwrap_or("").trim();
    match coalesced.abstract_text.as_deref() {
        Some(abs) => format!("{title}\n\n{abs}").trim_start().to_string(),
        None => title.to_string(),
    }
}

/// Extract an abstract section from a converted-markdown paper.
///
/// Algorithm:
/// 1. Find the first line that looks like an "Abstract" marker:
///    `## Abstract` / `# Abstract` / `**Abstract**` / bare `Abstract` /
///    bare `ABSTRACT`. Match within the first 8 KB of the document
///    (abstracts are always near the top — anchoring this way avoids
///    matching the word "abstract" inside the body or references).
/// 2. Capture content from after that line until either:
///    - The next markdown heading (`^#+\s+`)
///    - A standalone section-name line for a common IMRaD section
///      (`Introduction`, `Background`, `Methods`, `Results`, etc.)
///    - 4 KB past the start (hard cap; typical abstracts < 3 KB).
///    - End of document.
/// 3. Trim and return; caller decides if the result is long enough.
///
/// Returns `None` if no abstract marker is found in the first 8 KB.
pub fn extract_markdown_abstract(md: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;

    static ABSTRACT_MARKER: OnceLock<Regex> = OnceLock::new();
    static SECTION_BREAK: OnceLock<Regex> = OnceLock::new();

    let abstract_marker = ABSTRACT_MARKER.get_or_init(|| {
        // Match an "Abstract" marker on its own line:
        //   `## Abstract`, `# Abstract`, `### Abstract`
        //   `**Abstract**`, `**Abstract:**`
        //   `Abstract`, `ABSTRACT`, `Abstract:` (bare, no markup)
        // Trailing colon and optional `**` close are both allowed.
        Regex::new(r"(?im)^\s*(?:#{1,3}\s+)?(?:\*\*\s*)?abstract\s*[:.]?\s*(?:\*\*)?\s*$")
            .expect("ABSTRACT_MARKER regex")
    });
    let section_break = SECTION_BREAK.get_or_init(|| {
        // End of abstract: next heading or a standalone IMRaD section line.
        // The leading anchor `^` is line-mode courtesy of `(?m)`.
        Regex::new(
            r"(?im)^\s*(?:#{1,3}\s+|\*\*\s*)?(?:introduction|background|methods?|materials?\s+and\s+methods?|results?|discussion|conclusions?|keywords?|references?|acknowledg(?:e?)ments?|funding|author\s+contributions?|conflicts?\s+of\s+interest|1\.?\s+introduction|1\.?\s+background)\b",
        )
        .expect("SECTION_BREAK regex")
    });

    let head_len = md.len().min(8192);
    let head = &md[..head_len];
    let marker = abstract_marker.find(head)?;
    let start = marker.end();
    if start >= md.len() {
        return None;
    }

    // Skip leading whitespace/newlines after the marker.
    let body_start = start + md[start..].find(|c: char| !c.is_whitespace()).unwrap_or(0);
    if body_start >= md.len() {
        return None;
    }

    let cap_end = md.len().min(body_start + 4096);
    let body = &md[body_start..cap_end];

    let end_rel = section_break
        .find(body)
        .map(|m| m.start())
        .unwrap_or(body.len());
    let text = body[..end_rel].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_markdown_h2_abstract() {
        let md = "# Paper Title\n\n## Abstract\nThis study examines running mechanics in long-distance runners with chronic lower back pain. We measured spinal shrinkage in 25 athletes.\n\n## Introduction\nRunning is a complex motor task.\n";
        let got = extract_markdown_abstract(md).unwrap();
        assert!(got.contains("running mechanics"));
        assert!(!got.contains("Running is a complex"));
    }

    #[test]
    fn extracts_bare_abstract_line() {
        // The 46%-of-corpus pattern: `Abstract\n<paragraph>` with no marker.
        let md = "Genetic evidence of gender difference in autism\n\nYi Zhang, Na Li, Chao Li\n\nAbstract\nAutism spectrum disorder is a complex neurodevelopmental disorder with a male-to-female prevalence of 4:1. However, the genetic basis is unclear and warrants further investigation across cohorts.\n\nBackground\nASD prevalence varies.\n";
        let got = extract_markdown_abstract(md).unwrap();
        assert!(got.contains("Autism spectrum disorder"));
        assert!(!got.contains("ASD prevalence"));
    }

    #[test]
    fn extracts_allcaps_abstract() {
        let md = "Title Here\n\nABSTRACT\nThe authors report on a randomised controlled trial of running as treatment for chronic non-specific low back pain across multiple sites.\n\nINTRODUCTION\nBack pain affects millions.\n";
        let got = extract_markdown_abstract(md).unwrap();
        assert!(got.contains("randomised controlled trial"));
        assert!(!got.contains("Back pain affects"));
    }

    #[test]
    fn returns_none_when_no_marker() {
        let md = "Just a body paragraph with no abstract marker anywhere here. The word abstract appears later in references somewhere.";
        assert!(extract_markdown_abstract(md).is_none());
    }

    #[test]
    fn caps_at_4kb_runaway() {
        // Abstract with no section break — capped at 4 KB.
        let body = "Lorem ipsum ".repeat(1000);
        let md = format!("## Abstract\n{body}");
        let got = extract_markdown_abstract(&md).unwrap();
        assert!(got.len() <= 4096);
    }

    #[test]
    fn coalesce_prefers_openalex() {
        let r = coalesce_abstract(
            Some("This is a long-enough OpenAlex abstract that exceeds the minimum character threshold of one hundred chars for sure.".to_string()),
            Some("## Abstract\nDifferent markdown content here that is also more than one hundred characters long."),
        );
        assert_eq!(r.source, AbstractSource::Openalex);
        assert!(r.abstract_text.as_ref().unwrap().contains("OpenAlex"));
    }

    #[test]
    fn coalesce_falls_through_to_markdown() {
        let r = coalesce_abstract(
            None,
            Some("## Abstract\nMarkdown-derived abstract content that is plenty long for the minimum threshold check used by the coalesce.\n\n## Methods\nbody"),
        );
        assert_eq!(r.source, AbstractSource::Markdown);
        assert!(r.abstract_text.as_ref().unwrap().starts_with("Markdown"));
    }

    #[test]
    fn coalesce_falls_through_to_title_only() {
        let r = coalesce_abstract(
            Some("too short".to_string()),
            Some("no abstract marker here"),
        );
        assert_eq!(r.source, AbstractSource::TitleOnly);
        assert!(r.abstract_text.is_none());
    }

    #[test]
    fn build_embed_input_includes_title_and_abstract() {
        let r = CoalescedAbstract {
            source: AbstractSource::Openalex,
            abstract_text: Some("This is the abstract.".to_string()),
        };
        let s = build_embed_input(Some("Paper Title"), &r);
        assert_eq!(s, "Paper Title\n\nThis is the abstract.");
    }

    #[test]
    fn build_embed_input_title_only() {
        let r = CoalescedAbstract {
            source: AbstractSource::TitleOnly,
            abstract_text: None,
        };
        let s = build_embed_input(Some("Just a Title"), &r);
        assert_eq!(s, "Just a Title");
    }
}
