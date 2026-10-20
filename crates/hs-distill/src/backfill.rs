//! Backfill missing titles onto indexed points.
//!
//! A document's title is copied from its catalog row into the Qdrant
//! payload at index time (`extract_rule_based` in `metadata.rs`, then
//! `chunk_payload` in `qdrant.rs`), and a search hit reads it back out
//! of the payload. A document indexed *before* its catalog row existed
//! was therefore indexed with no title, and — because the corpus is not
//! re-indexed — it stays that way. Measured over the hits of the fixed
//! query set, 72 % of them came back with a null title, so an agent
//! that found a passage could not name the book it came from.
//!
//! The repair is a **payload update, not a re-embed**: the vector is
//! untouched, so this costs no GPU and no embedding model. It is also
//! why the title matters beyond display: DPR-style retrieval encodes
//! `title + body` (Chen et al. 2020, in the corpus), so a point
//! indexed without its title is a point the retriever cannot match by
//! title — exactly the kind of specific query the lexical channel
//! exists to serve.
//!
//! The decision logic is IO-free and separately testable: [`plan`]
//! decides what to write, [`TitleBackfill`] carries it out.

use std::collections::BTreeMap;

/// What to write onto one document's points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleBackfill {
    /// The document whose points are updated.
    pub doc_id: String,
    /// The title to set, when the catalog has one.
    pub title: Option<String>,
    /// The authors to set, when the catalog has them.
    pub authors: Option<Vec<String>>,
    /// The DOI to set, when the catalog has one.
    pub doi: Option<String>,
}

impl TitleBackfill {
    /// Is there anything to write for this document?
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.authors.is_none() && self.doi.is_none()
    }
}

/// Decide what to backfill, from the catalog, the documents' own text,
/// and what the payload has.
///
/// `payload` maps a doc_id to the title currently stored on its points
/// (`None` for a point with no title). `catalog` maps a doc_id to the
/// title the catalog holds (`None` for a document the catalog does not
/// know). `derived` maps a doc_id to a title taken from the document's own
/// text (`None` when it has none).
///
/// The catalog wins over the document's own text: it is a human- or
/// provider-supplied title, and the document's first line is a guess. But
/// most of the corpus has no catalog row at all — measured, 57 % of the
/// title-less documents — so the document's own text is the only source
/// left for most of them.
///
/// A document is written only when the payload is missing something. A
/// document neither source can title is reported as untitled, never given
/// a placeholder that would look like a title.
pub fn plan(
    payload: &BTreeMap<String, Option<String>>,
    catalog: &BTreeMap<String, Option<String>>,
    derived: &BTreeMap<String, Option<String>>,
) -> Vec<TitleBackfill> {
    let mut out = Vec::new();
    for (doc_id, current) in payload {
        let catalog_title = catalog
            .get(doc_id)
            .and_then(|t| t.as_ref())
            .filter(|t| !t.trim().is_empty());
        let derived_title = derived
            .get(doc_id)
            .and_then(|t| t.as_ref())
            .filter(|t| !t.trim().is_empty());
        let title = catalog_title.or(derived_title);
        let Some(title) = title else {
            // No source can title this document: it keeps what it has.
            continue;
        };
        // A title that is already stored is left alone, unless it can be
        // improved. A title that is *unusable* — the converter emitted a
        // table, or a line that repeats itself — is noise shown to the
        // user verbatim, and it is replaced by the catalog's title. A
        // title that merely *carries markup* (`<i>Arabidopsis</i>`) is
        // a good title with the converter's tags in it; it is replaced
        // only by a source that has the same title without the tags.
        if let Some(current) = current.as_ref().filter(|t| !t.trim().is_empty()) {
            let unusable = is_unusable_title(current);
            let marked = crate::metadata::has_markup(current);
            if !unusable && !marked {
                continue;
            }
            if catalog_title.is_none() {
                // The only source is the document's own text. A derived
                // title replaces an unusable one only if it is itself
                // clean; it never replaces a marked one, because the
                // derived title is the same text with the same markup.
                let derived_clean = derived_title
                    .map(|t| !crate::metadata::has_markup(t))
                    .unwrap_or(false);
                if !unusable || !derived_clean {
                    continue;
                }
            }
        }
        out.push(TitleBackfill {
            doc_id: doc_id.clone(),
            title: Some(title.clone()),
            authors: None,
            doi: None,
        });
    }
    out
}

/// Is this stored title one the converter's markup produced?
///
/// A title carrying HTML tags, or one that repeats itself, is an OCR
/// artifact: the converter lost the title page and emitted the page's
/// furniture instead. Shown to a user verbatim, it is noise.
///
/// The test lives in [`crate::metadata`], next to the rule that produces
/// such titles, so the two cannot drift apart.
pub fn is_unusable_title(title: &str) -> bool {
    crate::metadata::is_unusable_title(title)
}

/// The documents that have no title stored on their points.
///
/// These are the candidates for a backfill. Most of them are given a
/// title by [`plan`]; the rest have neither a catalog title nor a
/// title their own text yields, and stay untitled — reported, never
/// given a placeholder.
pub fn untitled(payload: &BTreeMap<String, Option<String>>) -> Vec<String> {
    payload
        .iter()
        .filter(|(_, t)| t.as_ref().is_none_or(|t| t.trim().is_empty()))
        .map(|(doc_id, _)| doc_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
            .collect()
    }

    #[test]
    fn a_document_with_a_title_is_not_touched() {
        let payload = map(&[("a", Some("A Title"))]);
        let catalog = map(&[("a", Some("A Different Title"))]);
        assert!(plan(&payload, &catalog, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_title_less_document_with_a_catalog_title_is_planned() {
        let payload = map(&[("a", None)]);
        let catalog = map(&[("a", Some("The Real Title"))]);
        let planned = plan(&payload, &catalog, &BTreeMap::new());
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].doc_id, "a");
        assert_eq!(planned[0].title.as_deref(), Some("The Real Title"));
    }

    #[test]
    fn the_catalog_title_wins_over_the_documents_own_text() {
        // The catalog title is supplied; the document's first line is a
        // guess.
        let payload = map(&[("a", None)]);
        let catalog = map(&[("a", Some("The Catalog Title"))]);
        let derived = map(&[("a", Some("The First Line"))]);
        let planned = plan(&payload, &catalog, &derived);
        assert_eq!(planned[0].title.as_deref(), Some("The Catalog Title"));
    }

    #[test]
    fn a_document_the_catalog_does_not_know_uses_its_own_text() {
        // Most of the corpus has no catalog row at all, so the document's
        // own text is the only source left.
        let payload = map(&[("a", None)]);
        let catalog = BTreeMap::new();
        let derived = map(&[("a", Some("The First Line"))]);
        let planned = plan(&payload, &catalog, &derived);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].title.as_deref(), Some("The First Line"));
    }

    #[test]
    fn a_stored_title_carrying_the_converters_markup_is_replaced() {
        // The converter emitted a table where the title page should be,
        // and that table became the title. The catalog holds the real
        // one, so the artifact is replaced.
        let payload = map(&[("a", Some("<table><tr><br>Some Heading"))]);
        let catalog = map(&[("a", Some("The Real Title"))]);
        let derived = BTreeMap::new();
        let planned = plan(&payload, &catalog, &derived);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].title.as_deref(), Some("The Real Title"));
    }

    #[test]
    fn a_stored_title_that_repeats_itself_is_replaced() {
        let payload = map(&[(
            "a",
            Some("and finally, and finally, and finally, and finally, and finally"),
        )]);
        let catalog = map(&[("a", Some("The Real Title"))]);
        let derived = BTreeMap::new();
        let planned = plan(&payload, &catalog, &derived);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].title.as_deref(), Some("The Real Title"));
    }

    #[test]
    fn a_good_stored_title_is_left_alone() {
        // The backfill must not churn: a document that already has a
        // title keeps it.
        let payload = map(&[("a", Some("A Perfectly Good Title"))]);
        let catalog = map(&[("a", Some("The Catalog Title"))]);
        let derived = BTreeMap::new();
        let planned = plan(&payload, &catalog, &derived);
        assert!(planned.is_empty());
    }

    #[test]
    fn a_stored_title_carrying_markup_inside_it_is_replaced_by_the_catalog() {
        // `<i>Arabidopsis</i>` is the converter's tag inside a good
        // title. The catalog holds the same title without the tag, so
        // the stored one is replaced.
        let payload = map(&[("a", Some("Stress in <i>Arabidopsis</i>"))]);
        let catalog = map(&[("a", Some("Stress in Arabidopsis"))]);
        let derived = BTreeMap::new();
        let planned = plan(&payload, &catalog, &derived);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].title.as_deref(), Some("Stress in Arabidopsis"));
    }

    #[test]
    fn a_marked_title_is_not_replaced_by_the_same_marked_title() {
        // The document's own text yields the same text with the same
        // markup. Writing it back would change nothing.
        let payload = map(&[("a", Some("Stress in <i>Arabidopsis</i>"))]);
        let catalog = BTreeMap::new();
        let derived = map(&[("a", Some("Stress in <i>Arabidopsis</i>"))]);
        let planned = plan(&payload, &catalog, &derived);
        assert!(planned.is_empty());
    }

    #[test]
    fn a_document_no_source_can_title_is_not_planned() {
        // There is no title to write, and inventing one would be a lie.
        let payload = map(&[("a", None)]);
        let catalog = BTreeMap::new();
        let derived = BTreeMap::new();
        assert!(plan(&payload, &catalog, &derived).is_empty());
    }

    #[test]
    fn an_empty_catalog_title_is_not_written() {
        let payload = map(&[("a", None)]);
        let catalog = map(&[("a", Some("   "))]);
        let derived = BTreeMap::new();
        assert!(plan(&payload, &catalog, &derived).is_empty());
    }

    #[test]
    fn a_whitespace_payload_title_counts_as_missing() {
        let payload = map(&[("a", Some("  "))]);
        let catalog = map(&[("a", Some("Real"))]);
        assert_eq!(plan(&payload, &catalog, &BTreeMap::new()).len(), 1);
    }

    #[test]
    fn untitled_lists_the_documents_nothing_can_fix() {
        let payload = map(&[("a", None), ("b", Some("Has One")), ("c", None)]);
        let untitled = untitled(&payload);
        assert_eq!(untitled.len(), 2);
        assert!(untitled.contains(&"a".to_string()));
        assert!(untitled.contains(&"c".to_string()));
        assert!(!untitled.contains(&"b".to_string()));
    }

    #[test]
    fn an_empty_backfill_has_nothing_to_write() {
        let empty = TitleBackfill {
            doc_id: "a".into(),
            title: None,
            authors: None,
            doi: None,
        };
        assert!(empty.is_empty());
        let with_title = TitleBackfill {
            title: Some("t".into()),
            ..empty
        };
        assert!(!with_title.is_empty());
    }
}
