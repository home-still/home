//! Coalesce + extract paper abstracts for the `paper_abstracts` Qdrant
//! collection.
//!
//! Three input sources, evaluated in order:
//! 1. **Local OpenAlex catalog** — `works.abstract_text` looked up by DOI.
//!    Plain reconstructed text (parser already inverted the
//!    `abstract_inverted_index` at ingest), highest quality.
//! 2. **Catalog-stored abstract** — captured from whichever provider served
//!    the paper at download time.
//! 3. **VLM-converted markdown** — heuristic extraction of an `Abstract`
//!    section from the converted paper. Coverage is partial: scribe's
//!    `markdown_generator` emits "abstract"-classified regions as plain
//!    paragraphs, so `## Abstract` only appears when the PDF's layout
//!    triggered the `paragraph_title` classifier on the heading line.
//!    Empirical hit rate on the existing corpus: ~24% `## Abstract`,
//!    ~46% bare `Abstract`-on-its-own-line.
//!
//! A paper none of them yields a usable abstract for is not embedded: the
//! collection holds abstracts, and a title-only vector would be a degraded
//! substitute that search could not tell apart from a real hit. The caller
//! skips such papers and says so.
//!
//! Coalesce is deterministic at index time (not a runtime fallback). The
//! chosen source is stamped into the catalog so downstream code can filter
//! by provenance.

use serde::{Deserialize, Serialize};

/// Minimum non-whitespace length below which a candidate abstract is
/// treated as garbage and the coalesce falls through to the next source.
/// Picked to reject degenerate one-line strings ("Abstract not available",
/// "See PDF.") while still accepting genuinely-short abstracts from older
/// papers.
pub const MIN_ABSTRACT_CHARS: usize = 100;

/// Provenance of the abstract used for one paper's embedding. Serialized
/// form is what gets stamped into `CatalogEntry::abstract_embed.source`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AbstractSource {
    /// `works.abstract_text` from the local OpenAlex DuckDB.
    Openalex,
    /// `CatalogEntry::abstract_text` — provider-provided abstract captured
    /// at `paper_download` time (OpenAlex API, Crossref, Semantic Scholar,
    /// arxiv, etc., whichever provider gave the hit). Used when the local
    /// OpenAlex snapshot doesn't cover the DOI but the catalog has an
    /// abstract from another provider.
    Catalog,
    /// Extracted from the converted markdown's Abstract section.
    Markdown,
}

impl AbstractSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AbstractSource::Openalex => "openalex",
            AbstractSource::Catalog => "catalog",
            AbstractSource::Markdown => "markdown",
        }
    }
}

/// A usable abstract and where it came from.
#[derive(Debug, Clone)]
pub struct CoalescedAbstract {
    pub source: AbstractSource,
    pub abstract_text: String,
}

impl CoalescedAbstract {
    pub fn abstract_chars(&self) -> u32 {
        self.abstract_text.chars().count() as u32
    }
}

/// Pick the best of (openalex catalog abstract, catalog-stored abstract,
/// markdown-extracted abstract), or `None` when none is usable. Each
/// candidate must be `>= MIN_ABSTRACT_CHARS` to be accepted; otherwise we
/// fall through.
///
/// Inputs are pre-fetched so this function stays IO-free and testable.
/// The caller (in the `hs distill abstracts build` flow) does the DOI
/// lookup against the OpenAlex DuckDB, reads `entry.abstract_text` from
/// the catalog, and fetches the markdown via the storage backend, then
/// passes the three candidates in. Source priority:
///
/// 1. `Openalex` — most authoritative; reconstructed plain text from the
///    OpenAlex snapshot.
/// 2. `Catalog` — provider-provided at download time; identical content to
///    (1) when OpenAlex was the provider, but also covers DOIs/papers not
///    in the local snapshot (Crossref, Semantic Scholar, arxiv...).
/// 3. `Markdown` — heuristic extraction from VLM-converted PDF.
pub fn coalesce_abstract(
    openalex_abstract: Option<String>,
    catalog_abstract: Option<String>,
    markdown_body: Option<&str>,
) -> Option<CoalescedAbstract> {
    if let Some(text) = openalex_abstract.filter(|s| s.trim().chars().count() >= MIN_ABSTRACT_CHARS)
    {
        return Some(CoalescedAbstract {
            source: AbstractSource::Openalex,
            abstract_text: text.trim().to_string(),
        });
    }

    if let Some(text) = catalog_abstract.filter(|s| s.trim().chars().count() >= MIN_ABSTRACT_CHARS)
    {
        return Some(CoalescedAbstract {
            source: AbstractSource::Catalog,
            abstract_text: text.trim().to_string(),
        });
    }

    markdown_body
        .and_then(extract_markdown_abstract)
        .filter(|s| s.chars().count() >= MIN_ABSTRACT_CHARS)
        .map(|extracted| CoalescedAbstract {
            source: AbstractSource::Markdown,
            abstract_text: extracted,
        })
}

/// Build the embedding input string from a coalesce result: the title as a
/// prefix — standard practice for paper-level embeddings (SPECTER/SciNCL
/// pretrain on `title [SEP] abstract`) — then the abstract.
pub fn build_embed_input(title: Option<&str>, coalesced: &CoalescedAbstract) -> String {
    let title = title.unwrap_or("").trim();
    format!("{title}\n\n{}", coalesced.abstract_text)
        .trim_start()
        .to_string()
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

    // Every byte offset taken from `md` is first floored to a UTF-8 char
    // boundary: `&str[..N]` panics when N lands inside a multi-byte char (a
    // Springer paper with an em-dash at byte 4096 crashed the abstracts
    // pipeline), and the head/cap lengths below are plain byte counts.
    use crate::text::floor_char_boundary as floor_boundary;

    let head_len = floor_boundary(md, md.len().min(8192));
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

    let cap_end = floor_boundary(md, md.len().min(body_start + 4096));
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
    fn does_not_panic_on_multibyte_at_cap() {
        // Reproduces the rc.325-bootstrap crash: a Springer page where an
        // em-dash (`–`, 3-byte UTF-8) straddles the 4096-byte cap boundary
        // and panicked the naive slice. `floor_boundary` should make this
        // safe.
        let prefix = "x".repeat(4094);
        // After 4094 "x"s the next char starts at byte 4094; pad with one
        // more "x", then put the em-dash so its 3 bytes span 4095..4098 —
        // straddling the 4096 cap.
        let md = format!("## Abstract\n{prefix}x–rest of abstract body that continues past the cap so the slice has to clip somewhere inside the em-dash.");
        let got = extract_markdown_abstract(&md).unwrap();
        // We just need it to NOT panic and return something non-empty.
        assert!(!got.is_empty());
        assert!(got.len() <= 4096);
    }

    #[test]
    fn coalesce_prefers_openalex() {
        let r = coalesce_abstract(
            Some("This is a long-enough OpenAlex abstract that exceeds the minimum character threshold of one hundred chars for sure.".to_string()),
            Some("Catalog-stored abstract from a different provider that should not win because OpenAlex is present and authoritative.".to_string()),
            Some("## Abstract\nDifferent markdown content here that is also more than one hundred characters long."),
        );
        let r = r.unwrap();
        assert_eq!(r.source, AbstractSource::Openalex);
        assert!(r.abstract_text.contains("OpenAlex"));
    }

    #[test]
    fn coalesce_falls_through_to_catalog() {
        // OpenAlex missing; catalog has a provider-supplied abstract.
        let r = coalesce_abstract(
            None,
            Some("Catalog-stored abstract from Semantic Scholar — plenty long for the minimum-chars threshold used by the coalesce.".to_string()),
            Some("## Abstract\nMarkdown-derived would win if catalog were missing but the catalog tier sits above markdown."),
        );
        let r = r.unwrap();
        assert_eq!(r.source, AbstractSource::Catalog);
        assert!(r.abstract_text.starts_with("Catalog-stored"));
    }

    #[test]
    fn coalesce_falls_through_to_markdown() {
        let r = coalesce_abstract(
            None,
            None,
            Some("## Abstract\nMarkdown-derived abstract content that is plenty long for the minimum threshold check used by the coalesce.\n\n## Methods\nbody"),
        );
        let r = r.unwrap();
        assert_eq!(r.source, AbstractSource::Markdown);
        assert!(r.abstract_text.starts_with("Markdown"));
    }

    #[test]
    fn coalesce_gates_on_chars_not_bytes() {
        // Equal *character* length must be accepted or rejected identically
        // regardless of script. A byte gate (the old bug) would accept a CJK
        // abstract while dropping a Latin one of the same char length.
        let cjk_100: String = "数".repeat(MIN_ABSTRACT_CHARS); // 100 chars, ~300 bytes
        let latin_100: String = "a".repeat(MIN_ABSTRACT_CHARS); // 100 chars, 100 bytes
        assert_eq!(
            coalesce_abstract(Some(cjk_100), None, None).unwrap().source,
            AbstractSource::Openalex
        );
        assert_eq!(
            coalesce_abstract(Some(latin_100), None, None)
                .unwrap()
                .source,
            AbstractSource::Openalex
        );

        // Just under the char floor → rejected for both scripts. The CJK case
        // (~297 bytes) would have wrongly passed a byte gate.
        let cjk_99: String = "数".repeat(MIN_ABSTRACT_CHARS - 1);
        let latin_99: String = "a".repeat(MIN_ABSTRACT_CHARS - 1);
        assert!(coalesce_abstract(Some(cjk_99), None, None).is_none());
        assert!(coalesce_abstract(Some(latin_99), None, None).is_none());
    }

    #[test]
    fn no_usable_abstract_yields_nothing_not_a_title_only_stand_in() {
        let r = coalesce_abstract(
            Some("too short".to_string()),
            Some("also short".to_string()),
            Some("no abstract marker here"),
        );
        assert!(r.is_none());
        assert!(coalesce_abstract(None, None, None).is_none());
    }

    #[test]
    fn a_too_short_markdown_abstract_is_not_accepted() {
        let md = "## Abstract\nSee PDF.\n\n## Introduction\nbody";
        assert!(coalesce_abstract(None, None, Some(md)).is_none());
    }

    #[test]
    fn build_embed_input_includes_title_and_abstract() {
        let r = CoalescedAbstract {
            source: AbstractSource::Openalex,
            abstract_text: "This is the abstract.".to_string(),
        };
        let s = build_embed_input(Some("Paper Title"), &r);
        assert_eq!(s, "Paper Title\n\nThis is the abstract.");
    }

    #[test]
    fn build_embed_input_without_a_title_is_the_abstract_alone() {
        let r = CoalescedAbstract {
            source: AbstractSource::Catalog,
            abstract_text: "Only the abstract.".to_string(),
        };
        assert_eq!(build_embed_input(None, &r), "Only the abstract.");
        assert_eq!(build_embed_input(Some("  "), &r), "Only the abstract.");
    }

    #[test]
    fn abstract_chars_counts_characters() {
        let r = CoalescedAbstract {
            source: AbstractSource::Markdown,
            abstract_text: "数据 abc".to_string(),
        };
        assert_eq!(r.abstract_chars(), 6);
    }

    // ── char-boundary safety of the byte-offset slicing ────────────────

    #[test]
    fn multibyte_char_straddling_the_8kb_head_window_does_not_panic() {
        // Byte 8192 falls inside a 3-byte char; the marker sits before it.
        let md = format!("## Abstract\n{}€ and more text", "a".repeat(8192 - 12 - 1));
        assert!(md.len() > 8192 && !md.is_char_boundary(8192));
        let got = extract_markdown_abstract(&md).unwrap();
        assert!(got.starts_with("aaa"));
    }

    #[test]
    fn marker_after_a_multibyte_char_at_the_head_window_is_ignored_not_a_panic() {
        // The marker lies wholly past the head window; nothing is found.
        let md = format!("{}€\nAbstract\n{}", "a".repeat(8191), "text ".repeat(40));
        assert!(!md.is_char_boundary(8192));
        assert!(extract_markdown_abstract(&md).is_none());
    }

    #[test]
    fn multibyte_text_before_and_inside_the_abstract_is_sliced_on_boundaries() {
        let md = format!(
            "Ünïcödé Títle — naïve façade\n\nAbstract\n{}\n\nIntroduction\nbody",
            "数据分析表明模型稳定。".repeat(20)
        );
        let got = extract_markdown_abstract(&md).unwrap();
        assert!(got.starts_with("数据"));
        assert!(!got.contains("body"));
    }
}
