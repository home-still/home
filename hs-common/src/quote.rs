//! Finding a quoted passage in a converted document, and on its page.
//!
//! A citation is checked in two steps.
//!
//! [`locate`] answers *does the document say this?* It looks for the quote
//! in the converted markdown: first as it was typed, then as the same
//! letters and digits in the same order, whatever the markup, the line
//! breaks and the typography between them. It never accepts an
//! approximation: a quote the document does not contain is an error that
//! names the closest passage, so the quote can be corrected from it.
//!
//! [`place`] answers *where on the page is it?* The markdown has no
//! positions, so the located passage is aligned against the words of the
//! source's page in the word-box sidecar ([`crate::word_boxes`]) and
//! [`line_rects`] turns the matching words into one rectangle per text
//! line. This step is tolerant, because the two texts were read by
//! different programs (a vision model wrote the markdown; the PDF's text
//! layer or character recognition gave the words): [`align`] is a
//! Smith–Waterman local alignment over tokens (Smith and Waterman,
//! "Identification of common molecular subsequences", 1981, DOI
//! 10.1016/0022-2836(81)90087-5), the page-local form of aligning
//! recognised text to a reference text (Yalniz and Manmatha, "A fast
//! alignment scheme for automatic OCR evaluation of books", 2011, DOI
//! 10.1109/ICDAR.2011.157). A placement that explains too few of the
//! passage's words is refused rather than drawn in the wrong place.
//!
//! # Tokens
//!
//! Both steps compare *tokens*: maximal runs of letters and digits, after
//! folding. Folding is Unicode compatibility decomposition with the
//! combining marks dropped, then lower case, so a ligature is its letters,
//! an accented letter is its base letter, and a typographic quote or dash
//! is a gap between tokens like any other punctuation. A word hyphenated
//! across a line break is one token.
//!
//! Markup is not text and yields no token: a markdown link's destination,
//! an image (its alternative text is not on the page), an HTML tag, and
//! the name of a LaTeX command (`\times`, `\mathbf`), whose argument is
//! still read.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use crate::markdown::PAGE_SEPARATOR;
use crate::word_boxes::{PageWords, WordBoxes, BOX_UNITS_PER_PAGE_SIDE};

/// The fewest tokens a quote may have. A shorter one matches too many
/// places to say which is meant.
pub const MIN_QUOTE_TOKENS: usize = 4;

/// The most characters a quote may have.
pub const MAX_QUOTE_CHARS: usize = 2_000;

/// The share of a passage's tokens that must be matched, in order, by a
/// page's words for the passage to be placed there.
pub const MIN_ALIGNED_TOKEN_RATIO: f32 = 0.8;

/// The shortest token that may match another approximately. Shorter
/// tokens are too alike for one edit to mean a misreading.
pub const FUZZY_TOKEN_MIN_CHARS: usize = 5;

/// How many character edits apart two tokens of at least
/// [`FUZZY_TOKEN_MIN_CHARS`] characters may be and still match: the room
/// left for a character recognition slip.
pub const FUZZY_TOKEN_MAX_EDITS: usize = 1;

/// What two matching tokens add to an alignment's score.
pub const MATCH_SCORE: i32 = 2;

/// What two differing tokens take from an alignment's score.
pub const MISMATCH_PENALTY: i32 = 1;

/// What a token present on one side only takes from an alignment's score.
pub const GAP_PENALTY: i32 = 1;

/// How much of the shorter of two boxes' heights must overlap vertically
/// for the boxes to be on the same text line.
pub const LINE_MERGE_MIN_VERTICAL_OVERLAP: f32 = 0.5;

/// How far a line's rectangle reaches past its words to the left and to
/// the right, in box units. Up and down it is not padded: a word's box
/// is as tall as its font, which already holds the room above and below
/// the letters, and in text set tight it reaches the lines beside it as
/// it is.
pub const LINE_SIDE_PADDING_UNITS: u16 = 20;

/// How far below a line the next one may start, in heights of the shorter
/// of the two, for the first one's rectangle to reach down to it. The
/// leading between the lines of a paragraph and the space between two
/// paragraphs are closed, so a passage is one region; the gap a figure
/// leaves is not.
pub const LINE_JOIN_MAX_GAP_HEIGHTS: f32 = 1.0;

/// The characters that hyphenate a word at the end of a line.
const LINE_BREAK_HYPHENS: [char; 3] = ['-', '\u{2010}', '\u{00AD}'];

/// Characters that sit inside a word without being part of it or ending
/// it: the soft hyphen, the zero-width joiners and the byte-order mark.
const WORD_INTERNAL_IGNORABLES: [char; 4] = ['\u{00AD}', '\u{200C}', '\u{200D}', '\u{FEFF}'];

/// A quote, found in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteMatch {
    /// Where the passage starts: a byte offset into the markdown, as a
    /// chunk's span is.
    pub char_start: usize,
    /// Where the passage ends: the byte offset just past it.
    pub char_end: usize,
    /// The markdown line the passage starts on, the first line being 1.
    pub line_start: u32,
    /// The markdown line the passage ends on.
    pub line_end: u32,
    /// The passage as the document has it: `markdown[char_start..char_end]`.
    pub text: String,
    /// Whether the quote was found as typed. `false` when it was found
    /// only as the same tokens in the same order.
    pub exact: bool,
}

/// The passage of a document that comes closest to a quote the document
/// does not contain.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosestPassage {
    /// The markdown line the passage starts on, the first line being 1.
    pub line_start: u32,
    /// The markdown line the passage ends on.
    pub line_end: u32,
    /// The passage as the document has it.
    pub text: String,
    /// The share of the quote's tokens the passage has, in order.
    pub ratio: f32,
}

/// A quote the document does not contain.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteNotFound {
    /// The passage that comes closest, when any token of the quote is in
    /// the document at all.
    pub closest: Option<ClosestPassage>,
}

/// Why a quote was not located.
#[derive(Debug, Clone, PartialEq)]
pub enum LocateError {
    /// The quote has fewer than [`MIN_QUOTE_TOKENS`] tokens.
    TooShort {
        /// How many tokens it has.
        tokens: usize,
    },
    /// The quote has more than [`MAX_QUOTE_CHARS`] characters.
    TooLong {
        /// How many characters it has.
        chars: usize,
    },
    /// The text does not contain the quote.
    NotFound(QuoteNotFound),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { tokens } => write!(
                f,
                "the quote has {tokens} word(s); it needs at least {MIN_QUOTE_TOKENS} to be told \
                 apart from the rest of the text"
            ),
            Self::TooLong { chars } => write!(
                f,
                "the quote is {chars} characters long; the most a quote may have is \
                 {MAX_QUOTE_CHARS}"
            ),
            Self::NotFound(QuoteNotFound { closest: None }) => f.write_str(
                "the text does not contain the quote, and no passage of it resembles the quote",
            ),
            Self::NotFound(QuoteNotFound {
                closest: Some(closest),
            }) => write!(
                f,
                "the text does not contain the quote; the closest passage (lines {}-{}, \
                 ratio {:.2} of the quote's words in order) is: {}",
                closest.line_start, closest.line_end, closest.ratio, closest.text
            ),
        }
    }
}

impl std::error::Error for LocateError {}

/// The part of a located passage that lies on one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSlice {
    /// The page's number, 1-based.
    pub page: u32,
    /// The passage's tokens on that page, in order.
    pub tokens: Vec<String>,
}

/// A run of a page's words in the word-box sidecar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordSpan {
    /// The index of the first word.
    pub start: u32,
    /// The index just past the last word.
    pub end: u32,
    /// The share of the passage's tokens the run matches, in order.
    pub matched_ratio: f32,
}

/// A located passage's words on one page of the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// The page's number, 1-based.
    pub page: u32,
    /// The passage's words on that page.
    pub span: WordSpan,
}

/// Find `quote` in `markdown`.
///
/// The quote is looked for as typed, then as a run of the document's
/// tokens that spell the quote's tokens, so markup, line breaks, spacing,
/// hyphenation, case and typography do not hide a passage, and nothing
/// else is forgiven: every letter and digit of the quote must be there,
/// in order, with none between them.
///
/// When the quote is in the document more than once: with `near_line`,
/// the occurrence nearest that line is the match, an as-typed one before
/// an equally near one that is not; without it, the first as-typed
/// occurrence, and failing that the first.
pub fn locate(
    markdown: &str,
    quote: &str,
    near_line: Option<u32>,
) -> Result<QuoteMatch, LocateError> {
    let quote = quote.trim();
    let chars = quote.chars().count();
    if chars > MAX_QUOTE_CHARS {
        return Err(LocateError::TooLong { chars });
    }
    let wanted = tokens(quote);
    if wanted.len() < MIN_QUOTE_TOKENS {
        return Err(LocateError::TooShort {
            tokens: wanted.len(),
        });
    }
    let lines = LineIndex::new(markdown);
    let found = |(passage, exact): (Range<usize>, bool)| {
        lines
            .matched(markdown, passage, exact)
            .ok_or(LocateError::NotFound(QuoteNotFound { closest: None }))
    };

    let mut as_typed = markdown
        .match_indices(quote)
        .map(|(start, matched)| (start..start + matched.len(), true))
        .fuse();
    if near_line.is_none() {
        // Nothing to weigh the occurrences by, so the document is only
        // read token by token when the quote is not in it as typed.
        if let Some(first) = as_typed.next() {
            return found(first);
        }
    }

    let document = tokens(markdown);
    let spelled = spelled_runs(&document, &wanted)
        .into_iter()
        .map(|passage| (passage, false));
    match lines.nearest(as_typed.chain(spelled), near_line) {
        Some(nearest) => found(nearest),
        None => Err(LocateError::NotFound(QuoteNotFound {
            closest: closest_passage(markdown, &lines, &document, &wanted),
        })),
    }
}

/// Split a located passage into the part on each page it touches.
///
/// The markdown's pages are the pieces between
/// [`PAGE_SEPARATOR`](crate::markdown::PAGE_SEPARATOR)s, and its page N is
/// the source's page N. A page the passage only touches with punctuation
/// or markup has no slice.
pub fn page_slices(markdown: &str, found: &QuoteMatch) -> Vec<PageSlice> {
    let mut slices = Vec::new();
    let mut page_start = 0;
    for (index, page) in markdown.split(PAGE_SEPARATOR).enumerate() {
        let page_end = page_start + page.len();
        let start = found.char_start.max(page_start);
        let end = found.char_end.min(page_end);
        if start < end {
            let on_page: Vec<String> = markdown
                .get(start..end)
                .map(tokens)
                .unwrap_or_default()
                .into_iter()
                .map(|token| token.text)
                .collect();
            if !on_page.is_empty() {
                slices.push(PageSlice {
                    page: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    tokens: on_page,
                });
            }
        }
        page_start = page_end + PAGE_SEPARATOR.len();
    }
    slices
}

/// Find the run of a page's words that says `tokens`.
///
/// `None` when the best run matches less than [`MIN_ALIGNED_TOKEN_RATIO`]
/// of the tokens: the passage is not on this page, or the page was read
/// too differently to say where.
pub fn align(page: &PageWords, tokens: &[String]) -> Option<WordSpan> {
    let on_page = page_tokens(page);
    let reference: Vec<&str> = on_page.iter().map(|token| token.text.as_str()).collect();
    let query: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let alignment = local_alignment(&reference, &query)?;
    let matched_ratio = alignment.matches as f32 / tokens.len() as f32;
    if matched_ratio < MIN_ALIGNED_TOKEN_RATIO {
        return None;
    }
    let first = on_page.get(alignment.start)?;
    let last = on_page.get(alignment.end.checked_sub(1)?)?;
    Some(WordSpan {
        start: first.first_word,
        end: last.last_word.checked_add(1)?,
        matched_ratio,
    })
}

/// One rectangle per text line of the run `words` of a page's words, each
/// `[x0, y0, x1, y1]` in box units: as tall as its words and
/// [`LINE_SIDE_PADDING_UNITS`] wider than them on each side, within the
/// page. A line with the next one just below it ([`is_next_line_below`])
/// reaches down to it, so the lines of a paragraph leave no gaps between
/// their rectangles. The run is cut at the page's last word; one that
/// starts past it has no rectangle.
pub fn line_rects(page: &PageWords, words: std::ops::Range<u32>) -> Vec<[u16; 4]> {
    let start = usize::try_from(words.start).unwrap_or(usize::MAX);
    let end = usize::try_from(words.end)
        .unwrap_or(usize::MAX)
        .min(page.boxes.len());
    let mut lines: Vec<[u16; 4]> = Vec::new();
    for word in page.boxes.get(start..end).unwrap_or_default() {
        match lines.last_mut() {
            Some(line) if same_line(*line, *word) => *line = union(*line, *word),
            _ => lines.push(*word),
        }
    }
    let following: Vec<[u16; 4]> = lines.iter().skip(1).copied().collect();
    for (line, next) in lines.iter_mut().zip(following) {
        if is_next_line_below(*line, next) {
            line[3] = next[1];
        }
    }
    lines.into_iter().map(widened).collect()
}

/// Place a located passage on the pages of its source.
///
/// When the markdown has as many pages as the sidecar, each page's part of
/// the passage is aligned on that page. A part of fewer than
/// [`MIN_QUOTE_TOKENS`] tokens (the last words before a page break, say)
/// could be anywhere on its page and is left out. When the page counts
/// differ, the markdown's pages say nothing about the source's, and the
/// whole passage is aligned on every page of the sidecar; the best page is
/// the placement.
///
/// The result is empty when the passage could not be placed: any part of
/// it failed to align, or every part was too short.
pub fn place(markdown: &str, found: &QuoteMatch, boxes: &WordBoxes) -> Vec<Placement> {
    let slices = page_slices(markdown, found);
    if markdown.split(PAGE_SEPARATOR).count() == boxes.pages.len() {
        place_page_by_page(&slices, boxes)
    } else {
        place_on_best_page(&slices, boxes)
    }
}

fn place_page_by_page(slices: &[PageSlice], boxes: &WordBoxes) -> Vec<Placement> {
    slices
        .iter()
        .filter(|slice| slice.tokens.len() >= MIN_QUOTE_TOKENS)
        .map(|slice| {
            let span = align(boxes.page(slice.page)?, &slice.tokens)?;
            Some(Placement {
                page: slice.page,
                span,
            })
        })
        .collect::<Option<Vec<Placement>>>()
        .unwrap_or_default()
}

fn place_on_best_page(slices: &[PageSlice], boxes: &WordBoxes) -> Vec<Placement> {
    let passage: Vec<String> = slices
        .iter()
        .flat_map(|slice| slice.tokens.iter().cloned())
        .collect();
    let mut best: Option<Placement> = None;
    for page in &boxes.pages {
        let Some(span) = align(page, &passage) else {
            continue;
        };
        if best.is_none_or(|held| span.matched_ratio > held.span.matched_ratio) {
            best = Some(Placement {
                page: page.page,
                span,
            });
        }
    }
    best.into_iter().collect()
}

/// A folded token and the bytes of the text it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    start: usize,
    end: usize,
}

/// The tokens of `text`, in order (see the module documentation).
fn tokens(text: &str) -> Vec<Token> {
    let mut found = Vec::new();
    let mut open: Option<Token> = None;
    let mut at = 0;
    while let Some(c) = text.get(at..).and_then(|rest| rest.chars().next()) {
        let next = at + c.len_utf8();
        let markup_end = match c {
            ']' => link_destination_end(text, next),
            '!' => image_end(text, next),
            '<' => html_tag_end(text, next),
            '\\' => latex_command_end(text, next),
            _ => None,
        };
        if let Some(resume) = markup_end {
            found.extend(open.take());
            at = resume;
            continue;
        }
        if open.is_some() && LINE_BREAK_HYPHENS.contains(&c) {
            if let Some(resume) = line_break_end(text, next) {
                at = resume;
                continue;
            }
        }
        if !WORD_INTERNAL_IGNORABLES.contains(&c) {
            for folded in fold(c) {
                if folded.is_alphanumeric() {
                    let token = open.get_or_insert_with(|| Token {
                        text: String::new(),
                        start: at,
                        end: next,
                    });
                    token.text.push(folded);
                    token.end = next;
                } else {
                    found.extend(open.take());
                }
            }
        }
        at = next;
    }
    found.extend(open);
    found
}

/// A character as tokens compare it: compatibility-decomposed, without
/// combining marks, in lower case. A combining mark folds to nothing.
fn fold(c: char) -> impl Iterator<Item = char> {
    std::iter::once(c)
        .nfkd()
        .filter(|decomposed| !is_combining_mark(*decomposed))
        .flat_map(char::to_lowercase)
        .filter(|lowered| !is_combining_mark(*lowered))
}

/// With `from` just past a `]`: where a markdown link's `(destination)`
/// ends, when one follows on the same line.
fn link_destination_end(text: &str, from: usize) -> Option<usize> {
    let destination = text.get(from..)?.strip_prefix('(')?;
    let mut depth = 1_usize;
    for (offset, c) in destination.char_indices() {
        match c {
            '\n' => return None,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from + '('.len_utf8() + offset + ')'.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// With `from` just past a `!`: where a markdown image ends, when
/// `[alternative text]` follows and closes on the same line. The image's
/// `(destination)`, when it has one, is part of it.
fn image_end(text: &str, from: usize) -> Option<usize> {
    let alternative = text.get(from..)?.strip_prefix('[')?;
    let close = alternative.split('\n').next()?.find(']')?;
    let after = from + '['.len_utf8() + close + ']'.len_utf8();
    Some(link_destination_end(text, after).unwrap_or(after))
}

/// With `from` just past a `<`: where an HTML tag ends, when what follows
/// is a tag name and the tag closes on the same line.
fn html_tag_end(text: &str, from: usize) -> Option<usize> {
    let rest = text.get(from..)?;
    let name = rest.strip_prefix('/').unwrap_or(rest);
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let close = rest.split('\n').next()?.find('>')?;
    Some(from + close + '>'.len_utf8())
}

/// With `from` just past a `\`: where a LaTeX command's name ends, when
/// letters follow. `\times2` is the command `\times` and the text `2`.
fn latex_command_end(text: &str, from: usize) -> Option<usize> {
    let rest = text.get(from..)?;
    let name = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    (name > 0).then_some(from + name)
}

/// With `from` just past a hyphen: where the word resumes on the next
/// line, when the hyphen ends its line and a letter or digit starts the
/// next one.
fn line_break_end(text: &str, from: usize) -> Option<usize> {
    let after_hyphen = text.get(from..)?.trim_start_matches([' ', '\t', '\r']);
    let next_line = after_hyphen
        .strip_prefix('\n')?
        .trim_start_matches([' ', '\t']);
    next_line.chars().next().filter(|c| c.is_alphanumeric())?;
    Some(text.len() - next_line.len())
}

/// Where each line of a text starts, for turning byte offsets into line
/// numbers.
struct LineIndex {
    /// The byte offset of every line break, in order.
    breaks: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        Self {
            breaks: text.match_indices('\n').map(|(at, _)| at).collect(),
        }
    }

    /// The 1-based number of the line holding byte `offset`.
    fn line_of(&self, offset: usize) -> u32 {
        let before = self.breaks.partition_point(|&at| at < offset);
        u32::try_from(before + 1).unwrap_or(u32::MAX)
    }

    /// The first and last line of a non-empty byte range.
    fn lines_of(&self, range: &Range<usize>) -> (u32, u32) {
        let first = self.line_of(range.start);
        let last = self.line_of(range.end.saturating_sub(1));
        (first, last.max(first))
    }

    /// Of several occurrences, each with whether it is as typed, the one
    /// nearest `near_line`, or the first when no line is given.
    fn nearest(
        &self,
        mut found: impl Iterator<Item = (Range<usize>, bool)>,
        near_line: Option<u32>,
    ) -> Option<(Range<usize>, bool)> {
        let Some(near) = near_line else {
            return found.next();
        };
        // `min_by_key` keeps the first of several equally near.
        found.min_by_key(|(passage, _)| {
            let (first, last) = self.lines_of(passage);
            if near < first {
                first - near
            } else {
                near.saturating_sub(last)
            }
        })
    }

    fn matched(&self, markdown: &str, found: Range<usize>, exact: bool) -> Option<QuoteMatch> {
        let (line_start, line_end) = self.lines_of(&found);
        Some(QuoteMatch {
            char_start: found.start,
            char_end: found.end,
            line_start,
            line_end,
            text: markdown.get(found)?.to_string(),
            exact,
        })
    }
}

/// Every run of whole document tokens that spells the wanted tokens, as
/// byte ranges of the document.
///
/// The run is compared letter by letter, not token by token, so a word
/// the document breaks differently from the quote ("state-of-the-art"
/// against "state of the art", "Figure2" against "Figure 2") still
/// matches; it must begin at the start of a document token and end at the
/// end of one, so a quote cannot be found inside a longer word.
fn spelled_runs(document: &[Token], wanted: &[Token]) -> Vec<Range<usize>> {
    let needle: String = wanted.iter().map(|token| token.text.as_str()).collect();
    let mut spelled = String::new();
    let mut starts = Vec::with_capacity(document.len());
    for token in document {
        starts.push(spelled.len());
        spelled.push_str(&token.text);
    }

    let mut runs = Vec::new();
    let mut from = 0;
    while let Some(rest) = spelled.get(from..) {
        let Some(offset) = rest.find(&needle) else {
            break;
        };
        let begin = from + offset;
        let end = begin + needle.len();
        let first = starts.binary_search(&begin).ok();
        let last = if end == spelled.len() {
            document.len().checked_sub(1)
        } else {
            starts
                .binary_search(&end)
                .ok()
                .and_then(|next| next.checked_sub(1))
        };
        if let (Some(first), Some(last)) = (
            first.and_then(|index| document.get(index)),
            last.and_then(|index| document.get(index)),
        ) {
            runs.push(first.start..last.end);
        }
        let step = spelled
            .get(begin..)
            .and_then(|tail| tail.chars().next())
            .map_or(1, char::len_utf8);
        from = begin + step;
    }
    runs
}

/// The passage of the document that aligns best with a quote it does not
/// contain.
fn closest_passage(
    markdown: &str,
    lines: &LineIndex,
    document: &[Token],
    wanted: &[Token],
) -> Option<ClosestPassage> {
    let reference: Vec<&str> = document.iter().map(|token| token.text.as_str()).collect();
    let query: Vec<&str> = wanted.iter().map(|token| token.text.as_str()).collect();
    let alignment = local_alignment(&reference, &query)?;
    let first = document.get(alignment.start)?;
    let last = document.get(alignment.end.checked_sub(1)?)?;
    let passage = first.start..last.end;
    let (line_start, line_end) = lines.lines_of(&passage);
    Some(ClosestPassage {
        line_start,
        line_end,
        text: markdown.get(passage)?.to_string(),
        ratio: alignment.matches as f32 / wanted.len() as f32,
    })
}

/// A token of a page's words, and the words it was read from: more than
/// one when a word is hyphenated across a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageToken {
    text: String,
    first_word: u32,
    last_word: u32,
}

/// The tokens of a page's words, in reading order.
fn page_tokens(page: &PageWords) -> Vec<PageToken> {
    let mut found: Vec<PageToken> = Vec::new();
    let mut continues_previous = false;
    for (index, word) in page.words.iter().enumerate() {
        let Ok(word_index) = u32::try_from(index) else {
            break;
        };
        let mut parts = tokens(word).into_iter().map(|token| token.text);
        let mut contributed = false;
        if continues_previous {
            if let Some(broken) = found.last_mut() {
                if let Some(rest) = parts.next() {
                    broken.text.push_str(&rest);
                    broken.last_word = word_index;
                    contributed = true;
                }
            }
        }
        let before = found.len();
        found.extend(parts.map(|text| PageToken {
            text,
            first_word: word_index,
            last_word: word_index,
        }));
        contributed |= found.len() > before;
        continues_previous = contributed && breaks_across_lines(page, index);
    }
    found
}

/// Whether the word at `index` is the first half of a word hyphenated at
/// the end of its line: it ends in a hyphen after a letter or digit, and
/// the next word is on another line.
fn breaks_across_lines(page: &PageWords, index: usize) -> bool {
    let Some(word) = page.words.get(index) else {
        return false;
    };
    let mut from_end = word.chars().rev();
    let hyphenated = from_end
        .next()
        .is_some_and(|last| LINE_BREAK_HYPHENS.contains(&last))
        && from_end.next().is_some_and(char::is_alphanumeric);
    match (page.boxes.get(index), page.boxes.get(index + 1)) {
        (Some(here), Some(next)) => hyphenated && !same_line(*here, *next),
        _ => false,
    }
}

/// Whether two boxes are on the same text line: they overlap vertically
/// by at least [`LINE_MERGE_MIN_VERTICAL_OVERLAP`] of the shorter one.
fn same_line(a: [u16; 4], b: [u16; 4]) -> bool {
    let [_, a_top, _, a_bottom] = a;
    let [_, b_top, _, b_bottom] = b;
    let top = a_top.max(b_top);
    let bottom = a_bottom.min(b_bottom);
    let shorter = a_bottom
        .saturating_sub(a_top)
        .min(b_bottom.saturating_sub(b_top));
    if shorter == 0 {
        // A box without height is on a line it lies within.
        return top <= bottom;
    }
    f32::from(bottom.saturating_sub(top)) / f32::from(shorter) >= LINE_MERGE_MIN_VERTICAL_OVERLAP
}

/// Whether `lower` is the text line just below `upper`: it starts under
/// it, no further down than [`LINE_JOIN_MAX_GAP_HEIGHTS`] allows, and the
/// two share a stretch across the page. The first line of the next column
/// is not: it starts above, and to one side.
fn is_next_line_below(upper: [u16; 4], lower: [u16; 4]) -> bool {
    let [upper_left, upper_top, upper_right, upper_bottom] = upper;
    let [lower_left, lower_top, lower_right, lower_bottom] = lower;
    let Some(gap) = lower_top.checked_sub(upper_bottom) else {
        // The two overlap already, or `lower` is not below at all.
        return false;
    };
    let shorter = upper_bottom
        .saturating_sub(upper_top)
        .min(lower_bottom.saturating_sub(lower_top));
    let share_a_stretch = upper_left.max(lower_left) < upper_right.min(lower_right);
    share_a_stretch && f32::from(gap) <= f32::from(shorter) * LINE_JOIN_MAX_GAP_HEIGHTS
}

/// The smallest box holding both.
fn union(a: [u16; 4], b: [u16; 4]) -> [u16; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}

/// A box grown by [`LINE_SIDE_PADDING_UNITS`] to the left and to the
/// right, within the page.
fn widened(bounds: [u16; 4]) -> [u16; 4] {
    let [x0, y0, x1, y1] = bounds;
    [
        x0.saturating_sub(LINE_SIDE_PADDING_UNITS),
        y0,
        x1.saturating_add(LINE_SIDE_PADDING_UNITS)
            .min(BOX_UNITS_PER_PAGE_SIDE),
        y1,
    ]
}

/// The best local alignment of a query against a reference: the run of
/// reference tokens `start..end` it covers and how many query tokens it
/// matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Alignment {
    start: usize,
    end: usize,
    matches: usize,
}

/// One cell of the alignment table: the best score of an alignment ending
/// here, where in the reference that alignment starts and how many tokens
/// it matches. A score of zero is "no alignment ends here".
#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    score: i32,
    start: usize,
    matches: usize,
}

/// Smith–Waterman local alignment of `query` against `reference` (DOI
/// 10.1016/0022-2836(81)90087-5), with [`MATCH_SCORE`],
/// [`MISMATCH_PENALTY`] and a linear [`GAP_PENALTY`]. Two tokens match
/// when they are [`similar`].
///
/// Only the best cell is wanted, not the path to it, so the table is kept
/// two rows at a time and each cell carries its alignment's start and
/// match count instead of a traceback. Of equally good alignments the one
/// ending first in the reference is returned. `None` when no token of the
/// query matches any of the reference.
fn local_alignment(reference: &[&str], query: &[&str]) -> Option<Alignment> {
    let similarity = Similarity::new(reference, query);
    let mut previous = vec![Cell::default(); query.len() + 1];
    let mut current = previous.clone();
    let mut best_score = 0;
    let mut best = None;
    for row in 0..reference.len() {
        for column in 0..query.len() {
            let matches = similarity.at(row, column);
            let diagonal = previous[column];
            let paired = Cell {
                score: diagonal.score
                    + if matches {
                        MATCH_SCORE
                    } else {
                        -MISMATCH_PENALTY
                    },
                start: if diagonal.score == 0 {
                    row
                } else {
                    diagonal.start
                },
                matches: diagonal.matches + usize::from(matches),
            };
            let reference_skipped = Cell {
                score: previous[column + 1].score - GAP_PENALTY,
                ..previous[column + 1]
            };
            let query_skipped = Cell {
                score: current[column].score - GAP_PENALTY,
                ..current[column]
            };
            // `max_by_key` keeps the last of several equally good, so a
            // pairing is preferred to a gap.
            let cell = [query_skipped, reference_skipped, paired]
                .into_iter()
                .max_by_key(|candidate| candidate.score)
                .filter(|candidate| candidate.score > 0)
                .unwrap_or_default();
            current[column + 1] = cell;
            if cell.score > best_score {
                best_score = cell.score;
                best = Some(Alignment {
                    start: cell.start,
                    end: row + 1,
                    matches: cell.matches,
                });
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    best
}

/// Which query tokens match which reference tokens, worked out once per
/// distinct pair so the alignment's inner loop is a table lookup.
struct Similarity {
    /// For each reference position, the index of its distinct token.
    reference: Vec<usize>,
    /// For each query position, the index of its distinct token.
    query: Vec<usize>,
    /// How many distinct reference tokens there are: the table's width.
    width: usize,
    /// Row per distinct query token, column per distinct reference token.
    table: Vec<bool>,
}

impl Similarity {
    fn new(reference: &[&str], query: &[&str]) -> Self {
        let (reference, reference_tokens) = distinct(reference);
        let (query, query_tokens) = distinct(query);
        let width = reference_tokens.len();
        let mut table = Vec::with_capacity(width * query_tokens.len());
        let mut scratch = Vec::new();
        for wanted in &query_tokens {
            for held in &reference_tokens {
                table.push(similar(wanted, held, &mut scratch));
            }
        }
        Self {
            reference,
            query,
            width,
            table,
        }
    }

    /// Whether the reference token at `reference` matches the query token
    /// at `query`.
    fn at(&self, reference: usize, query: usize) -> bool {
        match (self.reference.get(reference), self.query.get(query)) {
            (Some(held), Some(wanted)) => self
                .table
                .get(wanted * self.width + held)
                .copied()
                .unwrap_or(false),
            _ => false,
        }
    }
}

/// The distinct tokens of a sequence, as characters, and for each
/// position the index of its token among them.
fn distinct(tokens: &[&str]) -> (Vec<usize>, Vec<Vec<char>>) {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut kinds = Vec::new();
    let positions = tokens
        .iter()
        .map(|&token| {
            *seen.entry(token).or_insert_with(|| {
                kinds.push(token.chars().collect());
                kinds.len() - 1
            })
        })
        .collect();
    (positions, kinds)
}

/// Whether two tokens stand for the same word: they are identical, or
/// both have at least [`FUZZY_TOKEN_MIN_CHARS`] characters and are within
/// [`FUZZY_TOKEN_MAX_EDITS`] edits of each other.
fn similar(a: &[char], b: &[char], scratch: &mut Vec<usize>) -> bool {
    a == b
        || (a.len() >= FUZZY_TOKEN_MIN_CHARS
            && b.len() >= FUZZY_TOKEN_MIN_CHARS
            && edits_within(a, b, FUZZY_TOKEN_MAX_EDITS, scratch))
}

/// Whether the edit (Levenshtein) distance between two tokens is at most
/// `limit`. `scratch` is the distance table's one row, reused between
/// calls.
fn edits_within(a: &[char], b: &[char], limit: usize, scratch: &mut Vec<usize>) -> bool {
    if a.len().abs_diff(b.len()) > limit {
        return false;
    }
    scratch.clear();
    scratch.extend(0..=b.len());
    for (row, left) in a.iter().enumerate() {
        let mut diagonal = row;
        let mut previous = row + 1;
        let mut smallest = previous;
        for (cell, right) in scratch.iter_mut().skip(1).zip(b) {
            let above = *cell;
            let distance = (diagonal + usize::from(left != right))
                .min(above + 1)
                .min(previous + 1);
            *cell = distance;
            diagonal = above;
            previous = distance;
            smallest = smallest.min(distance);
        }
        if let Some(first) = scratch.first_mut() {
            *first = row + 1;
        }
        if smallest > limit {
            return false;
        }
    }
    scratch.last().is_some_and(|&distance| distance <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word_boxes::{WordOrigin, WORD_BOXES_VERSION};

    /// US Letter, in points.
    const PAGE_WIDTH_PT: f32 = 612.0;
    const PAGE_HEIGHT_PT: f32 = 792.0;
    /// The layout [`page`] gives its words, in box units.
    const FIRST_LINE_TOP: u16 = 1_000;
    const LINE_PITCH: u16 = 300;
    const LINE_HEIGHT: u16 = 200;
    const FIRST_WORD_LEFT: u16 = 1_000;
    const WORD_PITCH: u16 = 800;
    const WORD_WIDTH: u16 = 700;

    /// A page with one text line per entry of `lines`, its words laid out
    /// on a grid.
    fn page(number: u32, lines: &[&str]) -> PageWords {
        let mut page = PageWords::new(number, WordOrigin::TextLayer, PAGE_WIDTH_PT, PAGE_HEIGHT_PT);
        for (row, line) in lines.iter().enumerate() {
            let top = FIRST_LINE_TOP + LINE_PITCH * u16::try_from(row).unwrap();
            for (column, word) in line.split_whitespace().enumerate() {
                let left = FIRST_WORD_LEFT + WORD_PITCH * u16::try_from(column).unwrap();
                page.push(
                    word.to_string(),
                    [left, top, left + WORD_WIDTH, top + LINE_HEIGHT],
                );
            }
        }
        page
    }

    fn sidecar(pages: Vec<PageWords>) -> WordBoxes {
        WordBoxes {
            version: WORD_BOXES_VERSION,
            source_sha256: "0".repeat(64),
            pages,
        }
    }

    fn texts(found: &[Token]) -> Vec<&str> {
        found.iter().map(|token| token.text.as_str()).collect()
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn not_found(result: Result<QuoteMatch, LocateError>) -> QuoteNotFound {
        match result {
            Err(LocateError::NotFound(not_found)) => not_found,
            other => panic!("expected the quote not to be found, got {other:?}"),
        }
    }

    #[test]
    fn tokens_fold_case_ligatures_accents_and_typography() {
        let found = tokens("The ﬁrst “Müller–Lyer” illusion, n\u{303}o 2²!");
        assert_eq!(
            texts(&found),
            ["the", "first", "muller", "lyer", "illusion", "no", "22"]
        );
    }

    #[test]
    fn a_token_knows_the_bytes_it_was_read_from() {
        let text = "a **ﬁne** day";
        let found = tokens(text);
        assert_eq!(texts(&found), ["a", "fine", "day"]);
        assert_eq!(&text[found[1].start..found[1].end], "ﬁne");
    }

    #[test]
    fn a_word_hyphenated_across_a_line_break_is_one_token() {
        assert_eq!(
            texts(&tokens(
                "an exam-\n  ple, a soft\u{ad}hyphen and a well-known one"
            )),
            [
                "an",
                "example",
                "a",
                "softhyphen",
                "and",
                "a",
                "well",
                "known",
                "one"
            ]
        );
        // A dash at the end of a line that no word continues stays a gap.
        assert_eq!(texts(&tokens("yes-\n- no")), ["yes", "no"]);
    }

    #[test]
    fn link_destinations_and_html_tags_are_not_text() {
        assert_eq!(
            texts(&tokens(
                "see [the report](https://example.org/a_(b)?c=1) for<br/>more</p> if 1 < 2"
            )),
            ["see", "the", "report", "for", "more", "if", "1", "2"]
        );
        // Neither closes on its own line, so both are text.
        assert_eq!(
            texts(&tokens("a](b\nc) and <d\ne>")),
            ["a", "b", "c", "and", "d", "e"]
        );
    }

    #[test]
    fn an_image_is_not_text() {
        assert_eq!(
            texts(&tokens(
                "before ![Figure] between ![a chart](img/chart_(1).png) after"
            )),
            ["before", "between", "after"]
        );
        // An exclamation mark that opens no image is punctuation, and an
        // alternative text that does not close on its line is text.
        assert_eq!(
            texts(&tokens("stop! [not an image] and ![open\nended]")),
            ["stop", "not", "an", "image", "and", "open", "ended"]
        );
    }

    #[test]
    fn a_latex_command_name_is_not_text_but_its_argument_is() {
        assert_eq!(
            texts(&tokens(
                r"cameras with $ 4096 \times2668 $ and $ \mathbf{x}_{i}^{\prime} $ views"
            )),
            ["cameras", "with", "4096", "2668", "and", "x", "i", "views"]
        );
        // A backslash before anything but letters is punctuation.
        assert_eq!(
            texts(&tokens(r"50\% of a\_b \\ end\")),
            ["50", "of", "a", "b", "end"]
        );
    }

    #[test]
    fn a_verbatim_quote_is_found_where_it_is() {
        let markdown = "# Title\n\nThe cat sat on the mat.\nIt purred.";
        let found = locate(markdown, "  cat sat on the mat. ", None).unwrap();
        assert_eq!(
            found,
            QuoteMatch {
                char_start: 13,
                char_end: 32,
                line_start: 3,
                line_end: 3,
                text: "cat sat on the mat.".to_string(),
                exact: true,
            }
        );
        assert_eq!(&markdown[found.char_start..found.char_end], found.text);
    }

    #[test]
    fn a_quote_is_found_through_markup_line_breaks_and_typography() {
        let markdown =
            "Intro.\n\nThe **quick** brown ﬁsh\njumped “over” the lazy\u{2014}dog. The end.";
        let found = locate(
            markdown,
            "the quick brown fish jumped \"over\" the lazy - dog",
            None,
        )
        .unwrap();
        assert!(!found.exact);
        assert_eq!(
            found.text,
            "The **quick** brown ﬁsh\njumped “over” the lazy\u{2014}dog"
        );
        assert_eq!((found.line_start, found.line_end), (3, 4));
        assert_eq!(&markdown[found.char_start..found.char_end], found.text);
    }

    #[test]
    fn a_quote_is_found_whichever_side_breaks_a_word() {
        let hyphenated = locate(
            "It is an exam-\nple of state of the art work.",
            "an example of state-of-the-art work",
            None,
        )
        .unwrap();
        assert_eq!(hyphenated.text, "an exam-\nple of state of the art work");

        let run_together = locate(
            "As Figure2 shows, it works.",
            "Figure 2 shows it works",
            None,
        );
        assert_eq!(run_together.unwrap().text, "Figure2 shows, it works");
    }

    #[test]
    fn a_quote_of_rendered_math_is_found_in_its_latex() {
        let markdown = r"We use 70 cameras with $ 4 0 9 6 \times2 6 6 8 $ resolution at 30 fps.";
        let found = locate(markdown, "70 cameras with 4096 × 2668 resolution", None).unwrap();
        assert!(!found.exact);
        assert_eq!(
            found.text,
            r"70 cameras with $ 4 0 9 6 \times2 6 6 8 $ resolution"
        );
    }

    #[test]
    fn a_quote_is_not_found_inside_a_longer_word() {
        // Two spaces, so the quote is not in the document as typed.
        let markdown = "Here is another  example of text.";
        let missing = not_found(locate(markdown, "other example of text", None));
        let closest = missing.closest.unwrap();
        assert_eq!(closest.text, "example of text");
        assert_eq!(closest.ratio, 0.75);
    }

    #[test]
    fn of_several_occurrences_the_one_nearest_the_line_is_the_match() {
        let markdown =
            "one two three four\nfiller\nfiller\none two three four\nfiller\none  two three four";
        let quote = "one two three four";
        assert_eq!(locate(markdown, quote, None).unwrap().line_start, 1);
        assert_eq!(locate(markdown, quote, Some(3)).unwrap().line_start, 4);
        assert_eq!(locate(markdown, quote, Some(4)).unwrap().line_start, 4);
        // Equally near two occurrences: the first.
        let between = "one two three four\nfiller\none two three four";
        assert_eq!(locate(between, quote, Some(2)).unwrap().line_start, 1);
        // The occurrence on line 6 is not as typed (two spaces). Given its
        // line it is the match all the same; equally near an as-typed one,
        // it is not; and without a line it is only reached when the quote
        // is nowhere as typed.
        let spaced = locate(markdown, quote, Some(6)).unwrap();
        assert_eq!((spaced.line_start, spaced.exact), (6, false));
        assert_eq!(spaced.text, "one  two three four");
        let tied = locate(markdown, quote, Some(5)).unwrap();
        assert_eq!((tied.line_start, tied.exact), (4, true));
        let only_spaced = "filler\none  two three four\nfiller\none   two three four";
        assert_eq!(locate(only_spaced, quote, None).unwrap().line_start, 2);
        assert_eq!(locate(only_spaced, quote, Some(4)).unwrap().line_start, 4);
    }

    #[test]
    fn a_quote_too_short_or_too_long_is_refused() {
        assert_eq!(
            locate("one two three four", "one two three", None),
            Err(LocateError::TooShort { tokens: 3 })
        );
        assert_eq!(
            locate("one two three four", "— … —", None),
            Err(LocateError::TooShort { tokens: 0 })
        );
        let long = "word ".repeat(MAX_QUOTE_CHARS);
        assert_eq!(
            locate(&long, &long, None),
            Err(LocateError::TooLong {
                chars: long.trim().chars().count()
            })
        );
    }

    #[test]
    fn a_misquote_is_refused_with_the_closest_passage() {
        let markdown = "First line.\n\nThe cat sat on the\nwarm mat, purring.";
        let missing = not_found(locate(markdown, "the cat sat on the cold mat", None));
        let closest = missing.closest.clone().unwrap();
        assert_eq!(closest.text, "The cat sat on the\nwarm mat");
        assert_eq!((closest.line_start, closest.line_end), (3, 4));
        assert_eq!(closest.ratio, 6.0 / 7.0);
        let message = LocateError::NotFound(missing).to_string();
        assert!(message.contains("lines 3-4"), "{message}");
        assert!(message.contains("0.86"), "{message}");
    }

    #[test]
    fn a_quote_nothing_resembles_has_no_closest_passage() {
        let missing = not_found(locate("alpha beta gamma delta", "one two three four", None));
        assert_eq!(missing.closest, None);
    }

    #[test]
    fn page_slices_split_a_passage_at_the_page_break() {
        let markdown = format!(
            "page one starts here and ends with these words{PAGE_SEPARATOR}that carry on over \
             the break{PAGE_SEPARATOR}page three"
        );
        let within = locate(&markdown, "carry on over the break", None).unwrap();
        assert_eq!(
            page_slices(&markdown, &within),
            [PageSlice {
                page: 2,
                tokens: strings(&["carry", "on", "over", "the", "break"]),
            }]
        );

        let across = locate(&markdown, "with these words that carry on", None).unwrap();
        assert!(!across.exact);
        assert_eq!(
            page_slices(&markdown, &across),
            [
                PageSlice {
                    page: 1,
                    tokens: strings(&["with", "these", "words"]),
                },
                PageSlice {
                    page: 2,
                    tokens: strings(&["that", "carry", "on"]),
                },
            ]
        );
    }

    #[test]
    fn align_finds_the_words_that_say_a_passage() {
        let words = page(
            1,
            &["Running head 7", "The cat sat on", "the mat. It purred."],
        );
        let span = align(&words, &strings(&["cat", "sat", "on", "the", "mat"])).unwrap();
        assert_eq!((span.start, span.end), (4, 9));
        assert_eq!(span.matched_ratio, 1.0);
        assert_eq!(&words.words[4..9], ["cat", "sat", "on", "the", "mat."]);
    }

    #[test]
    fn align_reads_a_word_hyphenated_across_lines_as_one() {
        let words = page(1, &["It is an exam-", "ple of careful work."]);
        let passage = strings(&["an", "example", "of", "careful", "work"]);
        let span = align(&words, &passage).unwrap();
        assert_eq!((span.start, span.end), (2, 8));
        assert_eq!(span.matched_ratio, 1.0);

        // On one line the hyphen joins nothing: "exam-" and "ple" are two
        // words, and the passage's "example" matches neither.
        let one_line = page_tokens(&page(1, &["an exam- ple of work"]));
        let read: Vec<&str> = one_line.iter().map(|token| token.text.as_str()).collect();
        assert_eq!(read, ["an", "exam", "ple", "of", "work"]);
    }

    #[test]
    fn align_absorbs_recognition_slips() {
        // "Figure2" for "Figure 2", "sm0ke" for "smoke", a stray mark.
        let words = page(
            1,
            &[
                "Figure2 shows thin wisps. of",
                "sm0ke rising ~ slowly upward",
            ],
        );
        let passage = strings(&[
            "figure", "2", "shows", "thin", "wisps", "of", "smoke", "rising", "slowly", "upward",
        ]);
        let span = align(&words, &passage).unwrap();
        assert_eq!((span.start, span.end), (0, 10));
        assert_eq!(span.matched_ratio, 0.9);
    }

    #[test]
    fn align_refuses_a_passage_the_page_does_not_say() {
        let words = page(1, &["The cat sat on the mat"]);
        let half = strings(&["the", "cat", "sat", "by", "a", "warm", "fire"]);
        assert_eq!(align(&words, &half), None);
        assert_eq!(
            align(&words, &strings(&["dogs", "bark", "at", "night"])),
            None
        );
        assert_eq!(align(&words, &[]), None);
        assert_eq!(align(&page(1, &[]), &half), None);
    }

    #[test]
    fn line_rects_is_one_rectangle_per_text_line_wider_than_its_words() {
        let words = page(1, &["alpha beta gamma", "delta epsilon"]);
        // The first line reaches down to the second.
        assert_eq!(
            line_rects(&words, 1..5),
            [[1_780, 1_000, 3_320, 1_300], [980, 1_300, 2_520, 1_500]]
        );
    }

    #[test]
    fn a_line_reaches_down_only_to_the_line_just_below_it() {
        let first = [1_000, 1_000, 3_000, 1_200];
        let bottom_before = |second: [u16; 4]| {
            let mut words = PageWords::new(1, WordOrigin::Ocr, PAGE_WIDTH_PT, PAGE_HEIGHT_PT);
            words.push("first".to_string(), first);
            words.push("second".to_string(), second);
            let rects = line_rects(&words, 0..2);
            assert_eq!(rects.len(), 2, "{second:?}");
            rects[0][3]
        };
        // The next line of the paragraph, and the first of the one after
        // it, a line's height down.
        assert_eq!(bottom_before([1_000, 1_250, 3_000, 1_450]), 1_250);
        assert_eq!(bottom_before([1_000, 1_400, 3_000, 1_600]), 1_400);
        // Past a figure: further down than a line is tall.
        assert_eq!(bottom_before([1_000, 1_401, 3_000, 1_601]), 1_200);
        // The top of the next column: above, and to one side.
        assert_eq!(bottom_before([5_000, 300, 7_000, 500]), 1_200);
        // Below, and wholly to one side.
        assert_eq!(bottom_before([3_100, 1_250, 5_000, 1_450]), 1_200);
        // Set tight: the two overlap as they are.
        assert_eq!(bottom_before([1_000, 1_150, 3_000, 1_350]), 1_200);
    }

    #[test]
    fn line_rects_stays_within_the_page_and_the_words_there_are() {
        let mut edge = PageWords::new(1, WordOrigin::Ocr, PAGE_WIDTH_PT, PAGE_HEIGHT_PT);
        edge.push("edge".to_string(), [5, 5, 9_995, 9_995]);
        assert_eq!(
            line_rects(&edge, 0..7),
            [[0, 5, BOX_UNITS_PER_PAGE_SIDE, 9_995]]
        );
        assert!(line_rects(&edge, 3..5).is_empty());
    }

    #[test]
    fn a_passage_is_placed_on_its_own_page() {
        let markdown = format!(
            "The cat sat on the mat.{PAGE_SEPARATOR}Elsewhere the cat sat on the mat again."
        );
        let boxes = sidecar(vec![
            page(1, &["The cat sat on the mat."]),
            page(2, &["Elsewhere the cat sat", "on the mat again."]),
        ]);
        let found = locate(&markdown, "the cat sat on the mat again", None).unwrap();
        let placed = place(&markdown, &found, &boxes);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].page, 2);
        assert_eq!((placed[0].span.start, placed[0].span.end), (1, 8));
        let span = placed[0].span;
        assert_eq!(
            line_rects(&boxes.pages[1], span.start..span.end).len(),
            2,
            "the passage is on two text lines"
        );
    }

    #[test]
    fn a_passage_across_a_page_break_is_placed_on_both_pages() {
        let markdown =
            format!("It ends with these four words{PAGE_SEPARATOR}that carry on over the break.");
        let boxes = sidecar(vec![
            page(1, &["It ends with these four words", "3"]),
            page(2, &["Running head", "that carry on over the break."]),
        ]);
        let found = locate(&markdown, "with these four words that carry on over", None).unwrap();
        let placed = place(&markdown, &found, &boxes);
        let spans: Vec<(u32, u32, u32)> = placed
            .iter()
            .map(|placement| (placement.page, placement.span.start, placement.span.end))
            .collect();
        assert_eq!(spans, [(1, 2, 6), (2, 2, 6)]);

        // Two words on the first page could be anywhere on it, so only
        // the second page's part is placed.
        let short_end = locate(&markdown, "four words that carry on over", None).unwrap();
        let placed = place(&markdown, &short_end, &boxes);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].page, 2);

        // Neither part is long enough to place.
        let both_short = locate(&markdown, "four words that carry", None).unwrap();
        assert!(place(&markdown, &both_short, &boxes).is_empty());
    }

    #[test]
    fn an_image_before_a_page_break_does_not_stop_a_placement() {
        // The image's alternative text is in the markdown and on no page.
        let markdown =
            format!("![Figure] ![Figure]{PAGE_SEPARATOR}Existing fusion methods fail on hair.");
        let boxes = sidecar(vec![
            page(1, &["2"]),
            page(2, &["Existing fusion methods fail on hair."]),
        ]);
        let found = locate(
            &markdown,
            "![Figure] ![Figure] Existing fusion methods fail on hair",
            None,
        )
        .unwrap();
        let placed = place(&markdown, &found, &boxes);
        assert_eq!(placed.len(), 1);
        assert_eq!(
            (placed[0].page, placed[0].span.start, placed[0].span.end),
            (2, 0, 6)
        );
    }

    #[test]
    fn a_passage_the_page_does_not_say_is_not_placed() {
        let markdown = "The cat sat on the mat.";
        let boxes = sidecar(vec![page(1, &["An unrelated page of words"])]);
        let found = locate(markdown, "the cat sat on the mat", None).unwrap();
        assert!(place(markdown, &found, &boxes).is_empty());
    }

    #[test]
    fn with_differing_page_counts_the_best_page_of_the_sidecar_is_the_placement() {
        // The markdown lost its page breaks: one page against three.
        let markdown = "First page text here. The cat sat on the mat. Third page text here.";
        let boxes = sidecar(vec![
            page(1, &["First page text here."]),
            page(2, &["The cat sat on the mat."]),
            page(3, &["Third page text here."]),
        ]);
        let found = locate(markdown, "the cat sat on the mat", None).unwrap();
        let placed = place(markdown, &found, &boxes);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].page, 2);
        assert_eq!((placed[0].span.start, placed[0].span.end), (0, 6));

        let absent = locate(markdown, "first page text here", None).unwrap();
        let elsewhere = sidecar(vec![page(1, &["Nothing alike"]), page(2, &["at all"])]);
        assert!(place(markdown, &absent, &elsewhere).is_empty());
    }

    #[test]
    fn tokens_match_within_one_edit_only_when_long_enough() {
        let mut scratch = Vec::new();
        let mut alike = |a: &str, b: &str| {
            let a: Vec<char> = a.chars().collect();
            let b: Vec<char> = b.chars().collect();
            similar(&a, &b, &mut scratch)
        };
        assert!(alike("smoke", "smoke"));
        assert!(alike("smoke", "sm0ke"), "one substitution");
        assert!(alike("figure", "figure2"), "one insertion");
        assert!(alike("wisps", "wisp5"));
        assert!(!alike("smoke", "sm0k3"), "two substitutions");
        assert!(!alike("figure", "figures2"), "two insertions");
        assert!(!alike("cat", "cot"), "too short for an edit to be a slip");
        assert!(!alike("cats", "cat"));
        assert!(alike("at", "at"));
    }
}
