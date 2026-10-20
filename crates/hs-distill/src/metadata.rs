use std::time::Duration;

use hs_common::catalog::CatalogEntry;
use serde::Deserialize;

use crate::error::DistillError;
use crate::types::DocumentMeta;

/// Extract metadata from the catalog entry. The body text is deliberately
/// never mined for DOI or year — see the note below. Its first heading
/// *is* used, as a title, when the catalog has none.
pub fn extract_rule_based(
    markdown: &str,
    stem: &str,
    markdown_path: &str,
    catalog: Option<&CatalogEntry>,
) -> DocumentMeta {
    let mut meta = DocumentMeta {
        doc_id: stem.to_string(),
        markdown_path: markdown_path.to_string(),
        ..Default::default()
    };

    // Pull from catalog if available
    if let Some(cat) = catalog {
        meta.title = cat.title.clone();
        meta.authors = cat.authors.iter().map(|a| a.name.clone()).collect();
        meta.doi = cat.doi.clone();
        meta.publication_date = cat.publication_date.clone();
        meta.abstract_text = cat.abstract_text.clone();
        meta.cited_by_count = cat.cited_by_count;
        meta.source = cat.source.clone();
        meta.category = cat.category.clone();
        meta.original_format = cat.original_format.clone();
        meta.ingested_at = cat.downloaded_at.clone();
    }

    // Always derive pdf_path from the sharded storage key rather than the
    // catalog's `pdf_path` field — that field is a host-local filesystem
    // path (e.g. `/Users/.../papers/10/DOI.pdf`) written by whichever
    // machine ran the downloader, and leaking it through search results
    // exposes the downloader's home directory. The storage key is the
    // canonical location and is identical across hosts.
    meta.pdf_path = Some(format!("papers/{}", hs_common::sharded_key(stem, "pdf")));

    // A document with no catalog row still gets a title, taken from its
    // own first heading. A title-less point is not merely a display
    // gap: DPR-style retrieval encodes `title + body` (Chen et al. 2020),
    // so a point indexed without a title cannot be matched by title — and
    // an agent that finds a passage cannot name the document it came
    // from. 72 % of the hits of the fixed query set came back with a null
    // title for exactly this reason.
    //
    // The heading is used only when the catalog has none, and only when
    // it looks like a title: a heading that is a page number, a
    // "References" section, or a long sentence is not one, and a
    // placeholder that looks like a title is worse than no title.
    if meta.title.as_ref().is_none_or(|t| t.trim().is_empty()) {
        meta.title = title_from_markdown(markdown);
    }

    // Neither DOI nor year is ever regex-extracted from body text. The
    // first DOI-shaped string in a paper is almost always a reference
    // citation, not the paper's own DOI, which produced mislabeled chunks
    // in rc.<=230. The first 4-digit year in the opening lines has the
    // same problem and was worse, because it silently succeeded: journal
    // headers, copyright lines and the first citation all match, so a 2021
    // Frontiers paper was indexed as 2008. A wrong year is more damaging
    // than a missing one — it survives into search results and citations
    // with no signal that it was guessed. When the catalog has no
    // publication_date, the year stays None; backfill the catalog instead.

    meta
}

/// Bytes of document text shown to the model. Cut on a char boundary.
const LLM_SAMPLE_BYTES: usize = 2000;

/// The document's own title, taken from its first heading or first line.
///
/// Returns `None` when neither reads like a title. A heading that is a
/// page number, a bare section word ("References", "Abstract",
/// "Introduction"), or a long line is not a title, and returning one of
/// those would put a plausible-looking wrong value into every search
/// result that cited the document.
///
/// Both forms are tried because the corpus uses both: a markdown heading
/// (33 % of a sample) or the title as the first plain line (53 %). A
/// heading-only rule leaves two thirds of the documents without a title.
pub fn title_from_markdown(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let text = strip_markup(rest.trim_start_matches('#').trim());
            if looks_like_title(&text) {
                return Some(text);
            }
            // A heading that is not a title is not the title; do not fall
            // through to the body text below it.
            continue;
        }
        // A line that begins with a *table* is the converter's furniture
        // — the table's cells are not the document's title. A line that
        // begins with an inline tag (`<i>Arabidopsis</i> …`) is a good
        // title whose first word is italicised.
        if starts_with_a_table(trimmed) {
            break;
        }
        // The first plain line of a document that has no heading is its
        // title far more often than it is anything else.
        let text = strip_markup(trimmed);
        if looks_like_title(&text) && !text.ends_with('.') {
            return Some(text);
        }
        break;
    }
    None
}

/// Strip the HTML a converter left in the text.
///
/// The OCR of a PDF with a table or a footnote marker emits markup
/// (`<table><tr><br>`, `<scp>A</scp>`) where the title should be. A
/// title carrying markup is worse than no title: it is shown to the user
/// verbatim and it breaks the query that searches for the document's
/// name. The markup and its content are removed and the text around it
/// is kept, so a title that is really there is still found.
fn strip_markup(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // A tag that was never closed still swallowed the rest of the line.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this text repeat itself?
///
/// The OCR of a page with a header, a table and a caption can emit the
/// same words several times in one line ("and finally, and finally, and
/// finally"). Such a line is not a title, and it is the first line of
/// the document only because the converter lost the title page.
fn repeats_itself(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 8 {
        return false;
    }
    for size in [2usize, 3, 4, 5, 6] {
        if words.len() < 2 * size {
            continue;
        }
        for start in 0..=(words.len() - 2 * size) {
            let first = &words[start..start + size];
            if first.iter().any(|w| w.is_empty()) {
                continue;
            }
            // A run of one content word is not a repetition: "Body Image
            // and Sexual Dissatisfaction" repeats "and", and it is a
            // perfectly good title. A run of two or more words, or of
            // one word that carries meaning, is.
            if size < 2 && !is_meaningful_word(first[0]) {
                continue;
            }
            if words[start + size..start + 2 * size] == *first {
                return true;
            }
        }
    }
    false
}

/// Is this word one that carries meaning on its own?
///
/// "and", "the", "of" repeat in ordinary titles. A word like "modeling"
/// or "Intake" repeating is the converter's text running away.
fn is_meaningful_word(word: &str) -> bool {
    const STOP_WORDS: [&str; 25] = [
        "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "into", "of", "on",
        "or", "that", "the", "to", "was", "were", "with", "this", "these", "those", "its",
    ];
    let bare = word
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    !STOP_WORDS.contains(&bare.as_str())
}

/// Is this stored title one the converter's markup produced?
///
/// Two shapes are not a title at all: a line that **begins** with a tag
/// (the converter emitted a table where the title page should be), and
/// a line that repeats itself. Shown to a user verbatim, either is
/// noise.
///
/// A title that merely carries a tag *inside* it (`<i>Arabidopsis</i>`,
/// `CO<sub>2</sub>`) is a good title with markup in it. It is not
/// unusable — it is improvable, and the backfill replaces it when it
/// has a cleaner source.
pub fn is_unusable_title(title: &str) -> bool {
    if starts_with_a_table(title) {
        return true;
    }
    repeats_itself(title)
}

/// Does this line begin with a table?
///
/// The converter emits a table where the title page should be, and a
/// table's first tag is `<table>`. A line that begins with an *inline*
/// tag (`<i>Arabidopsis</i> …`) is a good title whose first word is
/// italicised, and it is not a table.
fn starts_with_a_table(title: &str) -> bool {
    let trimmed = title.trim_start();
    let lowered = trimmed.to_ascii_lowercase();
    lowered.starts_with("<table")
        || lowered.starts_with("<thead")
        || lowered.starts_with("<tbody")
        || lowered.starts_with("<tr")
        || lowered.starts_with("<td")
        || lowered.starts_with("<th")
}

/// Does this title carry markup inside it?
///
/// The markup is the converter's, not the title's. The text is right
/// and the tags are noise, so a title with markup can be replaced by a
/// clean one but is not wrong.
pub fn has_markup(title: &str) -> bool {
    title.contains('<') && title.contains('>')
}

/// Is this text usable as a title?
fn looks_like_title(text: &str) -> bool {
    // A title is a phrase, not a fragment and not a sentence.
    if text.len() < 4 || text.len() > 300 {
        return false;
    }
    // A bare section word names a part of the document, not the document.
    const NOT_TITLES: [&str; 12] = [
        "abstract",
        "introduction",
        "references",
        "bibliography",
        "contents",
        "acknowledgements",
        "acknowledgments",
        "appendix",
        "conclusion",
        "conclusions",
        "discussion",
        "summary",
    ];
    if NOT_TITLES
        .iter()
        .any(|w| text.trim_end_matches('.').eq_ignore_ascii_case(w))
    {
        return false;
    }
    // A heading that is mostly punctuation or digits is a page label.
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    // A title is a phrase, not a paragraph.
    if words.len() > 40 {
        return false;
    }
    // A line that repeats itself is an OCR artifact, not a title.
    if repeats_itself(text) {
        return false;
    }
    // At least one word with a capital letter and a letter: "chapter 3"
    // and "42" are not titles.
    words
        .iter()
        .any(|w| w.chars().any(|c| c.is_alphabetic()) && w.chars().any(|c| c.is_uppercase()))
}

/// Keywords and topics proposed by the metadata model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMetadata {
    pub keywords: Vec<String>,
    pub topics: Vec<String>,
}

/// Parse and check the configured Ollama base URL: http(s) with a host. An
/// invalid URL (including an unparseable port) is an error, never a default.
pub fn parse_ollama_url(ollama_url: &str) -> Result<reqwest::Url, DistillError> {
    let url = reqwest::Url::parse(ollama_url)
        .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?} is invalid: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(DistillError::Config(format!(
            "ollama_url {ollama_url:?} must be an http(s) URL with a host"
        )));
    }
    Ok(url)
}

fn llm_prompt(text_sample: &str) -> String {
    format!(
        "Extract 5-10 keywords and 2-3 academic topics from this text. \
         Return ONLY valid JSON: {{\"keywords\": [...], \"topics\": [...]}}\n\n\
         Text:\n{}\n\nJSON:",
        crate::text::prefix_at_most(text_sample, LLM_SAMPLE_BYTES)
    )
}

#[derive(Deserialize)]
struct GenerateReply {
    response: String,
}

#[derive(Deserialize)]
struct ExtractedMetadata {
    keywords: Vec<String>,
    topics: Vec<String>,
}

/// Parse the model's `response` text. It must be exactly the requested JSON
/// object; anything else is an error so a bad reply can never be mistaken
/// for "the model found no keywords".
fn parse_llm_reply(response: &str) -> Result<LlmMetadata, DistillError> {
    let parsed: ExtractedMetadata = serde_json::from_str(response.trim()).map_err(|e| {
        DistillError::Metadata(format!(
            "LLM reply is not the requested {{\"keywords\":[..],\"topics\":[..]}} object: {e}"
        ))
    })?;
    Ok(LlmMetadata {
        keywords: parsed.keywords,
        topics: parsed.topics,
    })
}

/// LLM-powered keyword/topic extraction via Ollama's `/api/generate`
/// (optional, `llm_metadata: true`). Every failure is an `Err`: unreachable
/// or slow server (`timeout` bounds the whole call), non-2xx status, or a
/// reply that is not the requested JSON.
pub async fn extract_llm_metadata(
    text_sample: &str,
    ollama_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<LlmMetadata, DistillError> {
    let endpoint = parse_ollama_url(ollama_url)?
        .join("api/generate")
        .map_err(|e| DistillError::Config(format!("ollama_url {ollama_url:?}: {e}")))?;

    let http = hs_common::http::client_builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .build()
        .map_err(|e| DistillError::Metadata(format!("failed to build Ollama client: {e}")))?;

    let resp = http
        .post(endpoint)
        .json(&serde_json::json!({
            "model": model,
            "prompt": llm_prompt(text_sample),
            "stream": false,
            "format": "json",
        }))
        .send()
        .await
        .map_err(|e| DistillError::Metadata(format!("Ollama request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(DistillError::Metadata(format!(
            "Ollama returned {status}: {}",
            crate::text::prefix_at_most(&body, 300)
        )));
    }
    let reply: GenerateReply = resp
        .json()
        .await
        .map_err(|e| DistillError::Metadata(format!("Ollama reply is not valid JSON: {e}")))?;
    parse_llm_reply(&reply.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_is_never_guessed_from_body_text() {
        // Opening lines carry a copyright year and a citation year that are
        // not the paper's own. With no catalog date, the year must stay
        // None rather than pick one of them.
        let markdown = "Frontiers in Psychology\n\nCopyright 2008 the authors\n\n\
                        See Smith et al. 1998 for background.\n\nAbstract";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.publication_date, None);
    }

    #[test]
    fn a_document_with_no_catalog_row_gets_its_title_from_its_first_heading() {
        // A title-less point cannot be matched by title, and an agent
        // cannot name the document it found a passage in.
        let markdown = "# On the Run for Hippocampal Plasticity\n\nSome body text.\n\n\
                        ## A section\n\nMore text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(
            meta.title.as_deref(),
            Some("On the Run for Hippocampal Plasticity")
        );
    }

    #[test]
    fn a_document_whose_title_is_its_first_plain_line_gets_that_title() {
        // More than half the corpus has no markdown heading and starts
        // with the title as a plain line.
        let markdown = "Real-Time Stress Detection by Means of Physiological Signals\n\n\
                        Authors et al.\n\nSome body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(
            meta.title.as_deref(),
            Some("Real-Time Stress Detection by Means of Physiological Signals")
        );
    }

    #[test]
    fn a_first_line_that_is_a_sentence_is_not_a_title() {
        // The first line of a document with no heading is often the
        // abstract's first sentence, not its title.
        let markdown =
            "In Shanghai, nitrogen pollution has become an increasingly serious problem.\n\n\
                        More body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.title, None);
    }

    #[test]
    fn a_first_line_carrying_the_converters_markup_is_not_a_title() {
        // The OCR of a PDF with a table emits the table where the title
        // page should be. A title that carries markup is shown to the
        // user verbatim, so it must not become the title.
        let markdown = "<table><tr><br>Some Heading Here</table>\n\n\
                        More body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.title, None);
    }

    #[test]
    fn a_first_line_that_repeats_itself_is_not_a_title() {
        // A page whose header, table and caption the OCR merged into one
        // line repeats itself. That is an artifact, not a title.
        let markdown = "The rapidity, and, and finally, and finally, and finally, and finally, \
                        finally, finally, finally, finally\n\nMore body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.title, None);
    }

    #[test]
    fn a_title_with_an_inline_tag_keeps_the_text_around_it() {
        // A subscript marker in the title is markup, but the title is
        // still there: the text around the tag is the title, and the
        // tag's own content is title text too.
        let markdown =
            "Computational authorship verification of a major 2nd century <scp>A</scp>\n\n\
                        More body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(
            meta.title.as_deref(),
            Some("Computational authorship verification of a major 2nd century A")
        );
    }

    #[test]
    fn a_title_shorter_than_the_repetition_window_is_still_judged() {
        // The repetition check compares runs of 4, 5 and 6 words. A
        // title shorter than twice the longest run has no second run to
        // compare, and the check must say so rather than read past the
        // end of the text.
        for text in [
            "and finally, and finally, and finally",
            "one two three four five six seven eight nine",
            "a b c d e f g h i j k l m n o p",
        ] {
            let _ = is_unusable_title(text);
        }
    }

    #[test]
    fn a_title_whose_first_word_is_italicised_is_still_a_title() {
        // The converter italicised the species name. The title is
        // there; only the tag is the converter's.
        let markdown =
            "<i>Juniperus communis</i> populations exhibit low variability in hydraulic safety\n\n\
                        More body text.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(
            meta.title.as_deref(),
            Some("Juniperus communis populations exhibit low variability in hydraulic safety")
        );
    }

    #[test]
    fn a_title_carrying_markup_is_unusable() {
        // A title that *begins* with a table is the converter's
        // furniture: the table's cells are not the document's title.
        assert!(is_unusable_title("<table><tr><br>Some Heading"));
        assert!(is_unusable_title("<table><tr><td>0</td><td>1</td>"));
        // A tag inside a title is not: the title is right, the tags are
        // the converter's. That includes a title whose *first* word is
        // italicised.
        assert!(!is_unusable_title("A <scp>A</scp> Title"));
        assert!(!is_unusable_title(
            "<i>Juniperus communis</i> populations exhibit low variability"
        ));
        assert!(has_markup("A <scp>A</scp> Title"));
        assert!(has_markup("The CB<sub>2</sub> receptor and its role"));
        assert!(!has_markup("A Perfectly Good Title"));
    }

    #[test]
    fn a_title_that_repeats_itself_is_unusable() {
        // The real corpus case: the OCR merged a header and a caption
        // into one line that repeats its words.
        assert!(is_unusable_title(
            "The rapidity, and, and finally, and finally, and finally, and finally"
        ));
        // A run of one content word is a repetition too.
        assert!(is_unusable_title(
            "Spine alignment in men during lateral sleep position: experimental study and modeling modeling modeling modeling"
        ));
    }

    #[test]
    fn a_title_that_repeats_only_a_stop_word_is_usable() {
        // "and" repeats in ordinary titles. That is not an artifact.
        assert!(!is_unusable_title(
            "Body Image and Sexual Dissatisfaction: Differences Among Heterosexual, Bisexual, and Lesbian Women"
        ));
    }

    #[test]
    fn a_good_title_is_usable() {
        assert!(!is_unusable_title(
            "Bipolar Disorder, Brain-Derived Neurotrophic Factor (BDNF) Val66Met Polymorphism"
        ));
    }

    #[test]
    fn the_catalog_title_wins_over_the_heading() {
        let cat = hs_common::catalog::CatalogEntry {
            title: Some("The Catalog Title".into()),
            ..Default::default()
        };
        let markdown = "# The Heading Title\n\nBody.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", Some(&cat));
        assert_eq!(meta.title.as_deref(), Some("The Catalog Title"));
    }

    #[test]
    fn a_heading_that_is_not_a_title_is_not_used() {
        // A bare section word names a part of the document, not the
        // document; a page number is not a title.
        for heading in ["# References", "# Abstract", "# 42", "# 7.1"] {
            let markdown = format!("{heading}\n\nBody text that is long enough.");
            let meta = extract_rule_based(&markdown, "stem", "path/stem.md", None);
            assert_eq!(meta.title, None, "heading {heading:?} was used as a title");
        }
    }

    #[test]
    fn a_document_with_no_heading_has_no_title() {
        let markdown = "Just some body text, no heading at all.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.title, None);
    }

    #[test]
    fn an_empty_catalog_title_falls_back_to_the_heading() {
        let cat = hs_common::catalog::CatalogEntry {
            title: Some("   ".into()),
            ..Default::default()
        };
        let markdown = "# A Real Heading\n\nBody.";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", Some(&cat));
        assert_eq!(meta.title.as_deref(), Some("A Real Heading"));
    }

    #[test]
    fn year_comes_from_the_catalog_when_present() {
        let cat = hs_common::catalog::CatalogEntry {
            publication_date: Some("2021-03-04".into()),
            ..Default::default()
        };
        let meta = extract_rule_based("Copyright 2008", "stem", "path/stem.md", Some(&cat));
        assert_eq!(meta.publication_date.as_deref(), Some("2021-03-04"));
    }

    #[test]
    fn doi_comes_only_from_catalog_not_body_text() {
        // Body text mentions another paper's DOI in a citation — must be ignored.
        let markdown = "... see 10.1002/aur.2049 for background ...";
        let meta = extract_rule_based(markdown, "stem", "path/stem.md", None);
        assert_eq!(meta.doi, None);
    }

    // ── LLM metadata (RA-13) ───────────────────────────────────────────

    fn lines_of(prompt: &str) -> &str {
        prompt
            .split("Text:\n")
            .nth(1)
            .and_then(|t| t.split("\n\nJSON:").next())
            .unwrap()
    }

    #[test]
    fn prompt_sample_never_splits_a_multibyte_char() {
        // "€" is 3 bytes, so byte 2000 lands inside a char for 2 of the 3
        // alignments. The old `&text[..text.len().min(2000)]` panicked.
        for lead in 0..3 {
            let text = format!("{}{}", "x".repeat(lead), "€".repeat(1000));
            let prompt = llm_prompt(&text);
            let sample = lines_of(&prompt);
            assert!(sample.len() <= LLM_SAMPLE_BYTES, "lead {lead}");
            assert!(
                sample.len() > LLM_SAMPLE_BYTES - 3,
                "lead {lead}: cut too early ({} bytes)",
                sample.len()
            );
            assert!(text.starts_with(sample));
        }
    }

    #[test]
    fn prompt_sample_keeps_short_text_whole() {
        let prompt = llm_prompt("café ☕");
        assert_eq!(lines_of(&prompt), "café ☕");
    }

    #[test]
    fn ollama_url_must_be_a_valid_http_url() {
        for bad in [
            "http://localhost:notaport",
            "http://localhost:99999",
            "localhost:11434",
            "ftp://localhost:11434",
            "",
        ] {
            assert!(
                matches!(parse_ollama_url(bad), Err(DistillError::Config(_))),
                "{bad:?} must be rejected"
            );
        }
        let ok = parse_ollama_url("http://127.0.0.1:11434").unwrap();
        assert_eq!(ok.port(), Some(11434));
    }

    #[test]
    fn reply_must_be_the_requested_object() {
        let ok = parse_llm_reply(r#" {"keywords":["a","b"],"topics":["t"]} "#).unwrap();
        assert_eq!(ok.keywords, ["a", "b"]);
        assert_eq!(ok.topics, ["t"]);

        for bad in [
            "I could not find any keywords",
            r#"{"keywords":["a"]}"#,
            r#"{"keywords":"a","topics":["t"]}"#,
            "```json\n{\"keywords\":[],\"topics\":[]}\n```",
            "",
        ] {
            assert!(
                matches!(parse_llm_reply(bad), Err(DistillError::Metadata(_))),
                "{bad:?} must not parse into metadata"
            );
        }
    }

    fn generate_reply(inner: &str) -> String {
        serde_json::json!({ "response": inner }).to_string()
    }

    #[tokio::test]
    async fn llm_extraction_posts_to_generate_and_returns_the_parsed_reply() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| {
            Reply::Json(
                200,
                generate_reply(r#"{"keywords":["k1","k2"],"topics":["t1"]}"#),
            )
        })
        .await;
        let got = extract_llm_metadata("héllo wörld", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got.keywords, ["k1", "k2"]);
        assert_eq!(got.topics, ["t1"]);

        let seen = fake.recorded();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].request_line.starts_with("POST /api/generate"));
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], false);
        assert_eq!(body["format"], "json");
        assert!(body["prompt"].as_str().unwrap().contains("héllo wörld"));
    }

    #[tokio::test]
    async fn unparseable_model_reply_is_an_error_not_empty_metadata() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Json(200, generate_reply("sorry, I cannot do that"))).await;
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
    }

    #[tokio::test]
    async fn ollama_error_status_is_an_error() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Json(404, r#"{"error":"model not found"}"#.into())).await;
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
        assert!(err.to_string().contains("model not found"), "{err}");
    }

    #[tokio::test]
    async fn stalled_ollama_hits_the_timeout() {
        use crate::testutil::{serve, Reply};
        let fake = serve(|_| Reply::Hang).await;
        let started = std::time::Instant::now();
        let err = extract_llm_metadata("text", &fake.url(), "m", Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Metadata(_)), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout did not bound the call"
        );
    }
}
