//! `documents`: what a store holds, newest first.
//!
//! Search finds passages; this lists documents. It reads every catalog
//! entry of the store on each call, which takes seconds on a store of
//! tens of thousands of documents, so the answer comes a page at a time
//! behind a cursor rather than as one list.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use hs_common::catalog::CatalogEntry;
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, StoreSpec};
use rmcp::schemars;

use super::{author_names, publication_year};
use crate::HomeStillMcp;

/// The documents one call returns unless it asks for another number.
pub const DOCUMENTS_DEFAULT_LIMIT: usize = 20;
/// The most documents one call returns.
pub const DOCUMENTS_MAX_LIMIT: usize = 200;
/// What separates the two parts of a cursor: when the last document of a
/// page was added, and its id. No timestamp holds one.
const CURSOR_SEPARATOR: char = '|';

/// The arguments of `documents`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct DocumentsParams {
    #[schemars(
        description = "The store to list: `academic` (default), `personal`, `web` or `syntheses`."
    )]
    pub store: Option<String>,
    #[schemars(description = "Only documents whose title contains this text, whatever its case.")]
    pub title_contains: Option<String>,
    #[schemars(
        description = "Only documents added after this moment: an RFC 3339 timestamp (`2026-11-01T00:00:00Z`) or a date (`2026-11-01`, read as the start of that day, UTC)."
    )]
    pub added_after: Option<String>,
    #[schemars(description = "Documents per page (default 20, at most 200).")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Where to go on from: `next_cursor` of the previous page, with the same filters."
    )]
    pub cursor: Option<String>,
}

/// One document of a store.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct DocumentRow {
    /// The document's id: what `read`, `cite` and `status` take.
    pub doc_id: String,
    /// The document's title.
    pub title: Option<String>,
    /// The document's authors, in the catalog entry's order.
    pub authors: Vec<String>,
    /// The year the document was published.
    pub year: Option<u16>,
    /// The document's DOI.
    pub doi: Option<String>,
    /// The address the document was captured from.
    pub source_url: Option<String>,
    /// The personal store's category of the document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// When the document was added to the store, RFC 3339.
    pub added: Option<String>,
    /// The document has been converted to text.
    pub converted: bool,
    /// The document's text is in the search index.
    pub indexed: bool,
}

/// One page of a store's documents.
#[derive(Debug, serde::Serialize)]
pub struct DocumentsPage {
    /// The store that was listed.
    pub store: String,
    /// The documents that pass the filters, on every page together.
    pub matching: usize,
    /// This page's documents, newest first.
    pub documents: Vec<DocumentRow>,
    /// What to pass as `cursor` for the next page. `None` on the last.
    pub next_cursor: Option<String>,
}

/// Where a document sorts: by when it was added, newest first, a document
/// with no date after every dated one, and by id among equals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    /// First, so that a document with no date sorts after every dated one.
    undated: bool,
    /// `Reverse`, so that a later moment sorts first.
    added: std::cmp::Reverse<Option<DateTime<Utc>>>,
    doc_id: String,
}

impl Position {
    fn new(added: Option<DateTime<Utc>>, doc_id: &str) -> Self {
        Self {
            undated: added.is_none(),
            added: std::cmp::Reverse(added),
            doc_id: doc_id.to_string(),
        }
    }

    /// The cursor that says "after this document".
    fn cursor(&self) -> String {
        let added = self.added.0.map(|at| at.to_rfc3339()).unwrap_or_default();
        format!("{added}{CURSOR_SEPARATOR}{}", self.doc_id)
    }

    fn from_cursor(cursor: &str) -> Result<Self, String> {
        let refused = || {
            format!("{cursor:?} is not a cursor this tool returned: pass `next_cursor` unchanged")
        };
        let (added, doc_id) = cursor.split_once(CURSOR_SEPARATOR).ok_or_else(refused)?;
        let added = if added.is_empty() {
            None
        } else {
            Some(
                DateTime::parse_from_rfc3339(added)
                    .map_err(|_| refused())?
                    .with_timezone(&Utc),
            )
        };
        Ok(Self::new(added, doc_id))
    }
}

/// The moment a catalog entry's `downloaded_at` names, when it is one.
fn added_at(entry: &CatalogEntry) -> Option<DateTime<Utc>> {
    let stamp = entry.downloaded_at.as_deref()?;
    DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// The moment `added_after` names: a timestamp, or the start of a date.
fn parse_added_after(input: &str) -> Result<DateTime<Utc>, String> {
    let input = input.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(input) {
        return Ok(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map(|date| date.and_time(NaiveTime::MIN).and_utc())
        .map_err(|_| {
            format!(
                "`added_after` {input:?} is neither an RFC 3339 timestamp \
                 (`2026-11-01T00:00:00Z`) nor a date (`2026-11-01`)"
            )
        })
}

fn row(doc_id: String, entry: CatalogEntry) -> DocumentRow {
    DocumentRow {
        doc_id,
        authors: author_names(&entry),
        year: entry.publication_date.as_deref().and_then(publication_year),
        converted: entry.conversion.is_some(),
        indexed: entry
            .embedding
            .as_ref()
            .is_some_and(|stamp| stamp.chunks_indexed > 0),
        title: entry.title,
        doi: entry.doi,
        source_url: entry.source_url,
        category: entry.category,
        added: entry.downloaded_at,
    }
}

/// The page of `entries` that `p` asks for.
fn page(
    store: &str,
    entries: Vec<(String, CatalogEntry)>,
    p: &DocumentsParams,
) -> Result<DocumentsPage, String> {
    let limit = p.limit.unwrap_or(DOCUMENTS_DEFAULT_LIMIT);
    if limit == 0 || limit > DOCUMENTS_MAX_LIMIT {
        return Err(format!(
            "`limit` is the documents per page, 1 to {DOCUMENTS_MAX_LIMIT}; got {limit}. \
             Follow `next_cursor` for more"
        ));
    }
    let added_after = p
        .added_after
        .as_deref()
        .map(parse_added_after)
        .transpose()?;
    let after = p.cursor.as_deref().map(Position::from_cursor).transpose()?;
    let title_contains = p.title_contains.as_deref().map(str::to_lowercase);

    let mut matching: Vec<(Position, String, CatalogEntry)> = entries
        .into_iter()
        .filter(|(_, entry)| match &title_contains {
            Some(wanted) => entry
                .title
                .as_deref()
                .is_some_and(|title| title.to_lowercase().contains(wanted)),
            None => true,
        })
        .filter_map(|(doc_id, entry)| {
            let added = added_at(&entry);
            if let Some(floor) = added_after {
                if added.is_none_or(|at| at <= floor) {
                    return None;
                }
            }
            Some((Position::new(added, &doc_id), doc_id, entry))
        })
        .collect();
    matching.sort_by(|a, b| a.0.cmp(&b.0));

    let total = matching.len();
    let mut rest = matching
        .into_iter()
        .filter(|(position, _, _)| after.as_ref().is_none_or(|after| position > after))
        .peekable();
    let mut documents = Vec::with_capacity(limit);
    let mut last = None;
    while documents.len() < limit {
        let Some((position, doc_id, entry)) = rest.next() else {
            break;
        };
        documents.push(row(doc_id, entry));
        last = Some(position);
    }
    let next_cursor = match (rest.peek(), last) {
        (Some(_), Some(last)) => Some(last.cursor()),
        _ => None,
    };
    Ok(DocumentsPage {
        store: store.to_string(),
        matching: total,
        documents,
        next_cursor,
    })
}

/// Every catalog entry of a store, by document id.
async fn entries(
    storage: &dyn Storage,
    store: &StoreSpec,
    layout: &DocumentLayout,
) -> Result<Vec<(String, CatalogEntry)>, String> {
    if store.name == hs_common::stores::PERSONAL {
        return personal::services::catalog::list_cards(storage, &layout.source_prefix)
            .await
            .map_err(|e| e.to_string());
    }
    let listed = hs_common::catalog::list_catalog_entries_parallel(
        storage,
        &layout.catalog_prefix,
        hs_common::catalog::CATALOG_FETCH_CONCURRENCY,
        |_, _| {},
    )
    .await
    .map_err(|e| format!("listing the catalog of {:?} failed: {e:#}", store.name))?;
    Ok(listed
        .into_iter()
        .map(|(doc_id, _, entry)| (doc_id, entry))
        .collect())
}

impl HomeStillMcp {
    /// List a store's documents, newest first.
    pub(crate) async fn list_documents(&self, p: DocumentsParams) -> Result<String, String> {
        let asked = p.store.as_deref().unwrap_or(hs_common::stores::ACADEMIC);
        let (holder, layout) = self.stores.documents(asked).map_err(|e| e.to_string())?;
        let entries = entries(&*self.storage, holder, layout).await?;
        crate::to_json(&page(holder.name, entries, &p)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, added: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            title: Some(title.to_string()),
            downloaded_at: added.map(str::to_string),
            ..CatalogEntry::default()
        }
    }

    fn shelf() -> Vec<(String, CatalogEntry)> {
        vec![
            (
                "old".into(),
                entry("Old Paper", Some("2026-01-01T00:00:00Z")),
            ),
            ("undated".into(), entry("Undated Paper", None)),
            (
                "new".into(),
                entry("New Report", Some("2026-03-01T00:00:00Z")),
            ),
            ("mid-b".into(), entry("Mid B", Some("2026-02-01T00:00:00Z"))),
            (
                "mid-a".into(),
                entry("Mid A", Some("2026-02-01T00:00:00+00:00")),
            ),
        ]
    }

    fn ids(page: &DocumentsPage) -> Vec<&str> {
        page.documents.iter().map(|d| d.doc_id.as_str()).collect()
    }

    #[test]
    fn documents_come_newest_first_with_the_undated_last() {
        let listed = page("academic", shelf(), &DocumentsParams::default()).expect("a page");
        assert_eq!(ids(&listed), ["new", "mid-a", "mid-b", "old", "undated"]);
        assert_eq!(listed.matching, 5);
        assert_eq!(listed.next_cursor, None);
    }

    #[test]
    fn the_cursor_walks_every_document_once() {
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let params = DocumentsParams {
                limit: Some(2),
                cursor: cursor.take(),
                ..DocumentsParams::default()
            };
            let listed = page("academic", shelf(), &params).expect("a page");
            assert_eq!(listed.matching, 5);
            seen.extend(listed.documents.into_iter().map(|d| d.doc_id));
            match listed.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, ["new", "mid-a", "mid-b", "old", "undated"]);
    }

    #[test]
    fn the_filters_keep_what_they_name() {
        let by_title = DocumentsParams {
            title_contains: Some("paper".into()),
            ..DocumentsParams::default()
        };
        let listed = page("academic", shelf(), &by_title).expect("a page");
        assert_eq!(ids(&listed), ["old", "undated"]);

        let by_date = DocumentsParams {
            added_after: Some("2026-02-01".into()),
            ..DocumentsParams::default()
        };
        let listed = page("academic", shelf(), &by_date).expect("a page");
        assert_eq!(ids(&listed), ["new"]);
    }

    #[test]
    fn a_limit_out_of_range_a_bad_date_and_a_foreign_cursor_are_refused() {
        for limit in [0, DOCUMENTS_MAX_LIMIT + 1] {
            let params = DocumentsParams {
                limit: Some(limit),
                ..DocumentsParams::default()
            };
            assert!(page("academic", shelf(), &params).is_err(), "limit {limit}");
        }
        let bad_date = DocumentsParams {
            added_after: Some("last week".into()),
            ..DocumentsParams::default()
        };
        assert!(page("academic", shelf(), &bad_date).is_err());
        let bad_cursor = DocumentsParams {
            cursor: Some("17".into()),
            ..DocumentsParams::default()
        };
        assert!(page("academic", shelf(), &bad_cursor).is_err());
    }
}
