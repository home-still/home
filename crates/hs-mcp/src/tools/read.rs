//! `read`: a document's catalog card and a slice of its converted text.
//!
//! The text comes back with the number of each line in front of it: the
//! numbers a search hit's `meta.line_start` and `cite`'s `near_line` use,
//! so a caller can go from a hit to its surroundings and from there to a
//! citation. A slice is bounded by `max_chars`; the answer says where to
//! go on from.

use hs_common::catalog::CatalogEntry;
use hs_common::markdown::PAGE_SEPARATOR;
use hs_common::storage::Storage;
use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;

use super::{author_names, catalog_entry, contents_result, object, publication_year};
use crate::links::DocumentLinks;
use crate::stem::Stem;
use crate::HomeStillMcp;

/// The most characters of text one call returns unless it asks for another
/// bound: about three thousand tokens, a few pages of a paper.
pub const READ_DEFAULT_MAX_CHARS: usize = 12_000;

/// The arguments of `read`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    #[schemars(
        description = "The store the document is in: the `store` of a hit. Default: `academic`. For `references` and `abstracts` the document read is the one the hit belongs to."
    )]
    pub store: Option<String>,
    #[schemars(
        description = "The document's id: `meta.doc_id` of a hit, or `doc_id` from `documents`."
    )]
    pub doc_id: Stem,
    #[schemars(description = "Read this page of the source (1-based). Not together with `lines`.")]
    pub page: Option<u32>,
    #[schemars(
        description = "Read these lines of the converted text, `[first, last]`, 1-based and inclusive: the numbers `meta.line_start` / `meta.line_end` of a hit and `quote.line_start` of a citation count in. Not together with `page`. Without `page` and `lines` the read starts at line 1."
    )]
    pub lines: Option<[u32; 2]>,
    #[schemars(
        description = "The most characters of text to return (default 12000). The slice ends on a whole line; `slice.next_line` is the line to go on from when the selection did not fit."
    )]
    pub max_chars: Option<usize>,
}

/// What a document's catalog entry says about it.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Card {
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
    /// The document's abstract, as the catalog entry holds it.
    #[serde(rename = "abstract")]
    pub abstract_text: Option<String>,
    /// The personal store's category of the document.
    pub category: Option<String>,
    /// When the document was added to the store, RFC 3339.
    pub added: Option<String>,
    /// The pages the conversion counted in the source.
    pub pages: Option<u64>,
}

impl Card {
    fn of(entry: CatalogEntry) -> Self {
        Self {
            authors: author_names(&entry),
            year: entry.publication_date.as_deref().and_then(publication_year),
            pages: entry.conversion.as_ref().map(|c| c.total_pages),
            title: entry.title,
            doi: entry.doi,
            source_url: entry.source_url,
            abstract_text: entry.abstract_text,
            category: entry.category,
            added: entry.downloaded_at,
        }
    }
}

/// Which part of the text an answer holds.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Slice {
    /// The first line returned.
    pub line_start: u32,
    /// The last line returned.
    pub line_end: u32,
    /// The page of the source the first line is on.
    pub page: u32,
    /// The lines the converted text has.
    pub total_lines: u32,
    /// The pages the converted text has.
    pub total_pages: u32,
    /// The one line returned is longer than `max_chars` and was cut.
    pub cut_mid_line: bool,
    /// The line to go on from, when the selection did not fit in
    /// `max_chars`. `None` when all of it was returned.
    pub next_line: Option<u32>,
}

/// What `read` answers, beside the text.
#[derive(Debug, serde::Serialize)]
pub struct ReadAnswer {
    /// The store that was asked.
    pub store: String,
    /// The document's id.
    pub doc_id: String,
    /// What the catalog entry says about the document.
    pub card: Card,
    /// Whether the document has been converted to text. When it has not,
    /// there is no slice and no text.
    pub converted: bool,
    /// Which part of the text follows.
    pub slice: Option<Slice>,
    /// Signed links that open the document.
    pub links: Option<DocumentLinks>,
}

/// An answer and the text that goes with it.
#[derive(Debug)]
pub struct Read {
    /// The card, the slice and the links.
    pub answer: ReadAnswer,
    /// The slice's lines, each behind its number.
    pub text: Option<String>,
}

impl Read {
    /// The result's content: the answer as JSON, then the numbered text.
    pub fn into_contents(self) -> Result<Vec<Content>, String> {
        let mut content = vec![Content::text(crate::to_json(&self.answer)?)];
        content.extend(self.text.map(Content::text));
        Ok(content)
    }
}

/// What the tool answers: the document, or the refusal as a tool error.
pub fn tool_result(outcome: Result<Read, String>) -> CallToolResult {
    contents_result(outcome.and_then(Read::into_contents))
}

/// The lines a caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// From line 1 on.
    Start,
    /// One page of the source.
    Page(u32),
    /// Lines `first..=last`.
    Lines(u32, u32),
}

impl Selection {
    fn parse(page: Option<u32>, lines: Option<[u32; 2]>) -> Result<Self, String> {
        match (page, lines) {
            (Some(_), Some(_)) => Err(
                "`page` and `lines` are two ways to say which part to read: give one of them"
                    .to_string(),
            ),
            (Some(page), None) => Ok(Self::Page(page)),
            (None, Some([first, last])) => Ok(Self::Lines(first, last)),
            (None, None) => Ok(Self::Start),
        }
    }
}

/// A converted text, by line and by page.
struct Text<'a> {
    /// The lines, without their line breaks.
    lines: Vec<&'a str>,
    /// The byte each line starts at.
    line_starts: Vec<usize>,
    /// The byte range of each page.
    pages: Vec<std::ops::Range<usize>>,
}

impl<'a> Text<'a> {
    fn new(markdown: &'a str) -> Self {
        let mut lines: Vec<&str> = markdown.split('\n').collect();
        // A text that ends with a line break has no line after it.
        if lines.len() > 1 && lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut at = 0;
        for line in &lines {
            line_starts.push(at);
            at += line.len() + 1;
        }
        let mut pages = Vec::new();
        let mut page_start = 0;
        for piece in markdown.split(PAGE_SEPARATOR) {
            let page_end = page_start + piece.len();
            pages.push(page_start..page_end);
            page_start = page_end + PAGE_SEPARATOR.len();
        }
        Self {
            lines,
            line_starts,
            pages,
        }
    }

    fn total_lines(&self) -> u32 {
        count(self.lines.len())
    }

    fn total_pages(&self) -> u32 {
        count(self.pages.len())
    }

    /// The 1-based number of the line holding byte `offset`.
    fn line_of(&self, offset: usize) -> u32 {
        count(self.line_starts.partition_point(|&start| start <= offset))
    }

    /// The 1-based number of the page line `line` is on: the last page
    /// that starts at or before the line.
    fn page_of(&self, line: u32) -> u32 {
        let start = self
            .line_starts
            .get(index(line))
            .copied()
            .unwrap_or_default();
        count(self.pages.partition_point(|page| page.start <= start)).max(1)
    }

    /// The first and last line of a selection, checked against the text.
    fn bounds(&self, selection: Selection) -> Result<(u32, u32), String> {
        let total = self.total_lines();
        match selection {
            Selection::Start => Ok((1, total)),
            Selection::Page(page) => {
                let range = page
                    .checked_sub(1)
                    .and_then(|at| self.pages.get(at as usize))
                    .ok_or_else(|| {
                        format!(
                            "there is no page {page}: the converted text has {} page(s)",
                            self.total_pages()
                        )
                    })?;
                let first = self.line_of(range.start);
                let last = self.line_of(range.end.saturating_sub(1).max(range.start));
                Ok((first, last.max(first)))
            }
            Selection::Lines(first, last) => {
                if first == 0 || last < first {
                    return Err(format!(
                        "`lines` is `[first, last]`, 1-based with first <= last; got [{first}, {last}]"
                    ));
                }
                if first > total {
                    return Err(format!(
                        "there is no line {first}: the converted text has {total} line(s)"
                    ));
                }
                Ok((first, last.min(total)))
            }
        }
    }

    /// The slice of a selection that fits in `max_chars`, and its lines
    /// behind their numbers.
    fn slice(&self, selection: Selection, max_chars: usize) -> Result<(Slice, String), String> {
        let (first, last) = self.bounds(selection)?;
        let mut text = String::new();
        let mut used = 0;
        let mut line_end = first;
        let mut cut_mid_line = false;
        for number in first..=last {
            let line = self.lines.get(index(number)).copied().unwrap_or_default();
            let chars = line.chars().count();
            if number > first && used + chars > max_chars {
                break;
            }
            let shown = if chars > max_chars {
                cut_mid_line = true;
                let end = line
                    .char_indices()
                    .nth(max_chars)
                    .map_or(line.len(), |(at, _)| at);
                &line[..end]
            } else {
                line
            };
            text.push_str(&format!("{number}: {shown}\n"));
            used += chars;
            line_end = number;
        }
        Ok((
            Slice {
                line_start: first,
                line_end,
                page: self.page_of(first),
                total_lines: self.total_lines(),
                total_pages: self.total_pages(),
                cut_mid_line,
                next_line: (line_end < last).then_some(line_end + 1),
            },
            text,
        ))
    }
}

/// A count of lines or pages as the answer carries it.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The index of a 1-based line.
fn index(line: u32) -> usize {
    (line as usize).saturating_sub(1)
}

impl HomeStillMcp {
    /// Read a document's card and a slice of its text.
    pub(crate) async fn read_document(&self, p: ReadParams) -> Result<Read, String> {
        let selection = Selection::parse(p.page, p.lines)?;
        let max_chars = p.max_chars.unwrap_or(READ_DEFAULT_MAX_CHARS);
        if max_chars == 0 {
            return Err("`max_chars` is the most text to return: give at least 1".to_string());
        }
        let asked = p.store.as_deref().unwrap_or(hs_common::stores::ACADEMIC);
        // A derived store's documents are its parent's.
        let (holder, layout) = self.stores.documents(asked).map_err(|e| e.to_string())?;
        let stem: &str = &p.doc_id;
        let storage: &dyn Storage = &*self.storage;

        let entry = catalog_entry(storage, holder, layout, stem).await?;
        let markdown = object(storage, &layout.markdown_key(stem))
            .await?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| format!("the converted text of {stem:?} is not UTF-8: {e}"))
            })
            .transpose()?;
        if entry.is_none() && markdown.is_none() {
            return Err(format!(
                "the store {:?} has no document {stem:?}: it has neither a catalog entry nor \
                 converted text. `documents` lists what the store holds",
                holder.name
            ));
        }

        let (slice, text) = match markdown.as_deref() {
            Some(markdown) => {
                let (slice, text) = Text::new(markdown).slice(selection, max_chars)?;
                (Some(slice), Some(text))
            }
            None => (None, None),
        };
        let links = self.link_minter.as_deref().and_then(|minter| {
            minter.mint(stem, holder.name, slice.as_ref().map(|s| s.page), None)
        });
        Ok(Read {
            answer: ReadAnswer {
                store: asked.to_string(),
                doc_id: stem.to_string(),
                card: Card::of(entry.unwrap_or_default()),
                converted: markdown.is_some(),
                slice,
                links,
            },
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_pages() -> String {
        ["one\ntwo", "three\n\nfive", "six"].join(PAGE_SEPARATOR)
    }

    #[test]
    fn lines_are_numbered_as_the_quote_locator_numbers_them() {
        let markdown = three_pages();
        let found = hs_common::quote::locate(
            "alpha beta gamma delta\nepsilon zeta eta theta\n",
            "epsilon zeta eta theta",
            None,
        )
        .expect("the quote is on line 2");
        assert_eq!(found.line_start, 2);

        let text = Text::new("alpha beta gamma delta\nepsilon zeta eta theta\n");
        assert_eq!(text.total_lines(), 2);
        let (slice, shown) = text
            .slice(Selection::Lines(2, 2), 100)
            .expect("line 2 exists");
        assert_eq!(shown, "2: epsilon zeta eta theta\n");
        assert_eq!((slice.line_start, slice.line_end), (2, 2));

        // one, two, "", ---, "", three, "", five, "", ---, "", six
        assert_eq!(Text::new(&markdown).total_lines(), 12);
    }

    #[test]
    fn a_page_is_the_lines_between_its_separators() {
        let markdown = three_pages();
        let text = Text::new(&markdown);
        assert_eq!(text.total_pages(), 3);

        let (slice, shown) = text.slice(Selection::Page(2), 100).expect("page 2 exists");
        assert_eq!(shown, "6: three\n7: \n8: five\n");
        assert_eq!(slice.page, 2);
        assert_eq!(slice.next_line, None);

        let (slice, shown) = text.slice(Selection::Page(3), 100).expect("page 3 exists");
        assert_eq!(shown, "12: six\n");
        assert_eq!(slice.page, 3);

        let refused = text.slice(Selection::Page(4), 100).unwrap_err();
        assert!(refused.contains("3 page(s)"), "{refused}");
        assert!(text.slice(Selection::Page(0), 100).is_err());
    }

    #[test]
    fn a_selection_that_does_not_fit_ends_on_a_whole_line_and_says_where_to_go_on() {
        let markdown = "aaaa\nbbbb\ncccc\n";
        let text = Text::new(markdown);

        let (slice, shown) = text.slice(Selection::Start, 9).expect("the text has lines");
        assert_eq!(shown, "1: aaaa\n2: bbbb\n");
        assert_eq!(slice.next_line, Some(3));
        assert!(!slice.cut_mid_line);

        let (slice, shown) = text
            .slice(Selection::Lines(3, 3), 9)
            .expect("line 3 exists");
        assert_eq!(shown, "3: cccc\n");
        assert_eq!(slice.next_line, None);
    }

    #[test]
    fn a_line_longer_than_the_bound_is_cut_and_flagged() {
        let text = Text::new("ünïcödé line\nnext");
        let (slice, shown) = text.slice(Selection::Start, 4).expect("the text has lines");
        assert_eq!(shown, "1: ünïc\n");
        assert!(slice.cut_mid_line);
        assert_eq!(slice.next_line, Some(2));
    }

    #[test]
    fn lines_past_the_end_are_refused_and_a_range_is_clamped() {
        let text = Text::new("a\nb\nc");
        let refused = text.slice(Selection::Lines(4, 9), 100).unwrap_err();
        assert!(refused.contains("3 line(s)"), "{refused}");
        assert!(text.slice(Selection::Lines(0, 2), 100).is_err());
        assert!(text.slice(Selection::Lines(3, 2), 100).is_err());

        let (slice, _) = text
            .slice(Selection::Lines(2, 9), 100)
            .expect("line 2 exists");
        assert_eq!(
            (slice.line_start, slice.line_end, slice.next_line),
            (2, 3, None)
        );
    }

    #[test]
    fn page_and_lines_together_are_refused() {
        assert!(Selection::parse(Some(1), Some([1, 2])).is_err());
        assert_eq!(Selection::parse(None, None), Ok(Selection::Start));
    }
}
