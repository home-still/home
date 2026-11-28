//! `synthesis_save`: keep an answer, with the passages it stands on.
//!
//! A synthesis is a document of the `syntheses` store: markdown whose
//! front matter lists the citations it was written from. Every citation is
//! checked against its document before anything is written, with the same
//! check `cite` makes, so a saved synthesis never points at words its
//! sources do not say.

use hs_common::catalog::CatalogEntry;
use rmcp::schemars;

use crate::stem::Stem;
use crate::HomeStillMcp;

/// The largest synthesis that is saved: 1 MiB of markdown is a long
/// report; more than that is not an answer to a question.
pub const MAX_SYNTHESIS_BYTES: usize = 1024 * 1024;
/// The hexadecimal characters of the SHA-256 of the title and the creation
/// time that start a synthesis's id, so two answers with one title do not
/// share an id.
pub const SYNTHESIS_STEM_HEX_CHARS: usize = 8;
/// The longest slug of the title in a synthesis's id.
const SYNTHESIS_SLUG_MAX_CHARS: usize = 60;
/// The extension a synthesis is stored under.
const SYNTHESIS_EXTENSION: &str = "md";
/// Who the announcement says stored the synthesis.
const EVENT_SOURCE: &str = "mcp:synthesis_save";

/// One passage a synthesis stands on.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SynthesisCitation {
    #[schemars(description = "The store the passage was found in: the `store` of the hit.")]
    pub store: String,
    #[schemars(description = "The document's id: `meta.doc_id` of the hit.")]
    pub doc_id: Stem,
    #[schemars(
        description = "The passage, word for word: what `cite` was given, or `quote.text` of its answer."
    )]
    pub quote: String,
}

/// The arguments of `synthesis_save`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SynthesisSaveParams {
    #[schemars(description = "The synthesis's title: the question it answers, or its subject.")]
    pub title: String,
    #[schemars(description = "The synthesis, as markdown, at most 1 MiB.")]
    pub body: String,
    #[schemars(
        description = "The passages the synthesis stands on, at least one. Each is checked against its document as `cite` checks it; if any one is not in its document, nothing is saved."
    )]
    pub citations: Vec<SynthesisCitation>,
    #[schemars(description = "Words to file the synthesis under.")]
    pub tags: Option<Vec<String>>,
}

/// A citation as the front matter records it, after it was checked.
#[derive(Debug, PartialEq, serde::Serialize)]
struct VerifiedCitation {
    store: String,
    doc_id: String,
    /// The page of the source the passage starts on, when that is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    /// The first and last line of the passage in the converted text.
    /// Absent for a passage of an abstract.
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<[u32; 2]>,
    /// The passage in the document's own words.
    quote: String,
}

/// What a synthesis says about itself, ahead of its text.
#[derive(Debug, serde::Serialize)]
struct FrontMatter<'a> {
    title: &'a str,
    created: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    tags: &'a [String],
    citations: &'a [VerifiedCitation],
}

/// What `synthesis_save` answers.
#[derive(Debug, serde::Serialize)]
pub struct Saved {
    /// The store the synthesis is in.
    pub store: &'static str,
    /// The synthesis's id: what `status`, `read` and `cite` take.
    pub doc_id: String,
    /// The citations that were checked and recorded.
    pub citations_verified: usize,
    /// What happens next.
    pub next: &'static str,
}

/// The title as part of an id: its ASCII letters and digits in lower case,
/// everything else a single `-`, at most [`SYNTHESIS_SLUG_MAX_CHARS`] long.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if slug.len() >= SYNTHESIS_SLUG_MAX_CHARS {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// The id of a synthesis titled `title`, created at `created`.
fn synthesis_stem(title: &str, created: &str) -> String {
    let mut stem = hs_common::word_boxes::source_sha256(format!("{title}\n{created}").as_bytes());
    stem.truncate(SYNTHESIS_STEM_HEX_CHARS);
    let slug = slug(title);
    if !slug.is_empty() {
        stem.push('-');
        stem.push_str(&slug);
    }
    stem
}

/// The stored document: the front matter, then the text.
fn document(front: &FrontMatter<'_>, body: &str) -> Result<String, String> {
    let yaml = serde_yaml_ng::to_string(front)
        .map_err(|e| format!("the synthesis's front matter could not be written: {e}"))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim()))
}

impl HomeStillMcp {
    /// Check a synthesis's citations and save it in the `syntheses` store.
    pub(crate) async fn save_synthesis(&self, p: SynthesisSaveParams) -> Result<String, String> {
        let title = p.title.trim();
        if title.is_empty() {
            return Err("`title` is empty: a synthesis is found again by its title".to_string());
        }
        if p.body.trim().is_empty() {
            return Err("`body` is empty: there is no synthesis to save".to_string());
        }
        if p.body.len() > MAX_SYNTHESIS_BYTES {
            return Err(format!(
                "`body` is {} bytes and a synthesis is at most {MAX_SYNTHESIS_BYTES}: nothing \
                 was saved",
                p.body.len()
            ));
        }
        if p.citations.is_empty() {
            return Err(
                "`citations` is empty: a synthesis is saved with the passages it stands on. \
                 Check each quoted claim with `cite`, then pass those quotes here"
                    .to_string(),
            );
        }
        self.require_event_bus("synthesis_save").await?;

        let mut verified = Vec::with_capacity(p.citations.len());
        for (at, citation) in p.citations.iter().enumerate() {
            let checked = self
                .check_quote(&citation.store, &citation.doc_id, &citation.quote, None)
                .await
                .map_err(|e| {
                    format!(
                        "citation {} of {} (store {:?}, document {:?}) did not pass, so nothing \
                         was saved: {e}",
                        at + 1,
                        p.citations.len(),
                        citation.store,
                        &*citation.doc_id,
                    )
                })?;
            verified.push(VerifiedCitation {
                store: citation.store.clone(),
                doc_id: citation.doc_id.to_string(),
                page: checked.page,
                lines: (!checked.from_abstract)
                    .then_some([checked.found.line_start, checked.found.line_end]),
                quote: checked.found.text,
            });
        }

        let created = chrono::Utc::now().to_rfc3339();
        let tags: Vec<String> = p
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect();
        let text = document(
            &FrontMatter {
                title,
                created: &created,
                tags: &tags,
                citations: &verified,
            },
            &p.body,
        )?;

        let store = hs_common::stores::SYNTHESES;
        let (_, layout) = self.stores.documents(store).map_err(|e| e.to_string())?;
        let stem = synthesis_stem(title, &created);
        hs_common::validate_stem(&stem)
            .map_err(|e| format!("the synthesis's id {stem:?} is not a usable name: {e:?}"))?;
        let entry = CatalogEntry {
            title: Some(title.to_string()),
            ..CatalogEntry::default()
        };
        self.store_document(
            layout,
            &stem,
            SYNTHESIS_EXTENSION,
            text.into_bytes(),
            entry,
            EVENT_SOURCE,
        )
        .await?;

        crate::to_json(&Saved {
            store,
            doc_id: stem,
            citations_verified: verified.len(),
            next: "The synthesis is queued for indexing. `status` with this `doc_id` and store \
                   `syntheses` says when it is indexed; then `recall` over `syntheses` \
                   finds it.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_a_hash_and_the_titles_slug() {
        let stem = synthesis_stem("Does RAG help? A 2024 review!", "2026-11-26T00:00:00+00:00");
        let (hash, slug) = stem.split_once('-').expect("a hash, a dash, a slug");
        assert_eq!(hash.len(), SYNTHESIS_STEM_HEX_CHARS);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{hash}");
        assert_eq!(slug, "does-rag-help-a-2024-review");
        assert!(hs_common::validate_stem(&stem).is_ok());

        let later = synthesis_stem("Does RAG help? A 2024 review!", "2026-11-27T00:00:00+00:00");
        assert_ne!(stem, later, "two answers with one title do not share an id");
    }

    #[test]
    fn a_title_with_no_ascii_letters_leaves_the_hash_alone() {
        let stem = synthesis_stem("检索增强生成", "2026-11-26T00:00:00+00:00");
        assert_eq!(stem.len(), SYNTHESIS_STEM_HEX_CHARS);
        assert!(hs_common::validate_stem(&stem).is_ok());
    }

    #[test]
    fn a_long_title_is_cut_without_a_trailing_dash() {
        let slug = slug(&"word ".repeat(40));
        assert!(slug.len() <= SYNTHESIS_SLUG_MAX_CHARS, "{}", slug.len());
        assert!(!slug.ends_with('-') && !slug.starts_with('-'), "{slug}");
    }

    #[test]
    fn the_front_matter_round_trips_and_the_text_follows_it() {
        let citations = vec![VerifiedCitation {
            store: "academic".into(),
            doc_id: "10.1000_x".into(),
            page: Some(3),
            lines: Some([120, 124]),
            quote: "a quote: with \"marks\"\nand a second line".into(),
        }];
        let tags = vec!["rag".to_string()];
        let text = document(
            &FrontMatter {
                title: "T: a title",
                created: "2026-11-26T00:00:00+00:00",
                tags: &tags,
                citations: &citations,
            },
            "\nThe answer.\n",
        )
        .expect("a document");

        let rest = text.strip_prefix("---\n").expect("front matter first");
        let (yaml, body) = rest.split_once("\n---\n\n").expect("front matter closes");
        assert_eq!(body, "The answer.\n");
        let front: serde_json::Value = serde_yaml_ng::from_str(yaml).expect("valid YAML");
        assert_eq!(front["title"], "T: a title");
        assert_eq!(front["tags"][0], "rag");
        assert_eq!(front["citations"][0]["lines"][1], 124);
        assert_eq!(
            front["citations"][0]["quote"],
            "a quote: with \"marks\"\nand a second line"
        );
    }
}
