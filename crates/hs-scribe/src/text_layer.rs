//! The words of a page's text layer and where each one sits.
//!
//! pdfium reports a page's characters in content order, each with the
//! rectangle it occupies. [`PdfParser::page_words`] groups them into words
//! and converts every rectangle to the units of the word-box sidecar
//! ([`hs_common::word_boxes`]) through pdfium's own page-to-device
//! transform, the one the renderer draws the page with, so a box lands on
//! the same glyphs in a rendered page whatever the page's rotation or crop.
//!
//! A word ends at whitespace, at a change of baseline and at a gap wider
//! than [`WORD_GAP_FRACTION_OF_CHAR_HEIGHT`] of the character before it. A
//! word broken across two lines stays two words, the first one keeping its
//! hyphen: the sidecar records what is on the page, line by line.

use anyhow::Result;
use hs_common::word_boxes::{PageWords, WordOrigin, BOX_UNITS_PER_PAGE_SIDE};
use pdfium_render::prelude::*;

use crate::classify::{ConvertFailure, FailureCode};
use crate::pdfium::{page_error, PdfParser};

/// Widest gap between two characters of one word, as a fraction of the
/// height of the first one's box. The space between two words of running
/// text is narrower than this and is reported by pdfium as a character of
/// its own; this bound separates what pdfium does not: the cells of a
/// table row, a number set against the far margin.
pub const WORD_GAP_FRACTION_OF_CHAR_HEIGHT: f32 = 0.3;

/// Furthest the baseline may move between two characters of one word, as a
/// fraction of the height of the first one's box. A superscript stays with
/// its word; the next line does not.
pub const BASELINE_SHIFT_FRACTION_OF_CHAR_HEIGHT: f32 = 0.5;

/// What the hyphen at the end of a broken line is recorded as. pdfium
/// replaces the character with a marker of its own.
const LINE_END_HYPHEN: char = '-';

/// One character of the text layer that is part of a word.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Glyph {
    character: char,
    /// Its loose bounds in page points: `[left, bottom, right, top]`.
    bounds_pt: [f32; 4],
    /// Where its baseline starts, in page points.
    origin_pt: [f32; 2],
    /// The direction it is written in, counter-clockwise from the page's
    /// x axis.
    angle_rad: f32,
    /// Its box in sidecar units.
    unit_box: [u16; 4],
}

/// The extent of `glyph`'s box along `axis`, a function of a point.
fn extent(glyph: &Glyph, axis: impl Fn(f32, f32) -> f32) -> (f32, f32) {
    let [left, bottom, right, top] = glyph.bounds_pt;
    let corners = [
        axis(left, bottom),
        axis(right, bottom),
        axis(left, top),
        axis(right, top),
    ];
    corners
        .into_iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), at| {
            (low.min(at), high.max(at))
        })
}

/// Whether `next` continues the word `last` is the latest character of:
/// measured in `last`'s own frame (along its writing direction and across
/// it), so text set at any angle is judged the same way.
fn same_word(last: &Glyph, next: &Glyph) -> bool {
    let (sin, cos) = last.angle_rad.sin_cos();
    let along = |x: f32, y: f32| x * cos + y * sin;
    let across = |x: f32, y: f32| y * cos - x * sin;

    let (last_low, last_high) = extent(last, across);
    let height = last_high - last_low;
    let (last_start, last_end) = extent(last, along);
    let (next_start, next_end) = extent(next, along);
    // The distance between the two boxes along the line, whichever comes
    // first (right-to-left scripts advance the other way).
    let gap = (next_start - last_end).max(last_start - next_end);
    let shift = (across(next.origin_pt[0], next.origin_pt[1])
        - across(last.origin_pt[0], last.origin_pt[1]))
    .abs();

    gap <= WORD_GAP_FRACTION_OF_CHAR_HEIGHT * height
        && shift <= BASELINE_SHIFT_FRACTION_OF_CHAR_HEIGHT * height
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

/// Group a page's characters into words. `None` is a character that is in
/// no word (whitespace, a control character, a character off the page) and
/// ends the word before it.
fn group_words(characters: impl IntoIterator<Item = Option<Glyph>>) -> Vec<(String, [u16; 4])> {
    let mut words = Vec::new();
    let mut open: Option<(String, [u16; 4], Glyph)> = None;
    for character in characters {
        let Some(glyph) = character else {
            words.extend(open.take().map(|(text, bounds, _)| (text, bounds)));
            continue;
        };
        match open.as_mut() {
            Some((text, bounds, last)) if same_word(last, &glyph) => {
                text.push(glyph.character);
                *bounds = union(*bounds, glyph.unit_box);
                *last = glyph;
            }
            _ => {
                words.extend(open.take().map(|(text, bounds, _)| (text, bounds)));
                open = Some((glyph.character.to_string(), glyph.unit_box, glyph));
            }
        }
    }
    words.extend(open.map(|(text, bounds, _)| (text, bounds)));
    words
}

/// Whether `character` is part of no word: whitespace, a control character
/// or one of the code points Unicode reserves as not a character.
fn separates_words(character: char) -> bool {
    const NONCHARACTER_LOW_BITS: u32 = 0xFFFE;
    character.is_whitespace()
        || character.is_control()
        || (u32::from(character) & NONCHARACTER_LOW_BITS) == NONCHARACTER_LOW_BITS
}

/// Read one character of the text layer: `None` when it is part of no word.
fn read_glyph(
    page: &PdfPage,
    character: &PdfPageTextChar,
    to_units: &PdfRenderConfig,
) -> Result<Option<Glyph>, PdfiumError> {
    let symbol = if character.is_hyphen()? {
        Some(LINE_END_HYPHEN)
    } else {
        character.unicode_char()
    };
    let Some(symbol) = symbol.filter(|symbol| !separates_words(*symbol)) else {
        return Ok(None);
    };

    let bounds = character.loose_bounds()?;
    let (left_top_x, left_top_y) = page.points_to_pixels(bounds.left(), bounds.top(), to_units)?;
    let (right_bottom_x, right_bottom_y) =
        page.points_to_pixels(bounds.right(), bounds.bottom(), to_units)?;
    let side = i32::from(BOX_UNITS_PER_PAGE_SIDE);
    let (x_low, x_high) = (
        left_top_x.min(right_bottom_x),
        left_top_x.max(right_bottom_x),
    );
    let (y_low, y_high) = (
        left_top_y.min(right_bottom_y),
        left_top_y.max(right_bottom_y),
    );
    // Text outside the page (cropped away) is in no rendering of it.
    if x_high <= 0 || y_high <= 0 || x_low >= side || y_low >= side {
        return Ok(None);
    }
    // In 0..=10_000 after the clamp, so the conversion is exact.
    let on_page = |unit: i32| unit.clamp(0, side) as u16;

    let (origin_x, origin_y) = character.origin()?;
    Ok(Some(Glyph {
        character: symbol,
        bounds_pt: [
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        ],
        origin_pt: [origin_x.value, origin_y.value],
        angle_rad: character.angle_radians()?,
        unit_box: [
            on_page(x_low),
            on_page(y_low),
            on_page(x_high),
            on_page(y_high),
        ],
    }))
}

/// A text layer pdfium opened and cannot read: a verdict on the document.
fn text_layer_error(e: PdfiumError, page_index: u16, character: usize) -> anyhow::Error {
    ConvertFailure::err(
        FailureCode::PdfParseError,
        format!("page {page_index}: character {character} of the text layer cannot be read: {e:?}"),
    )
}

impl PdfParser {
    /// The words of page `page_index` (0-based) as its text layer has them,
    /// in content order, each with its box in sidecar units. A page with no
    /// text layer (a scan) has no words; that is not an error.
    pub fn page_words(document: &PdfDocument, page_index: u16) -> Result<PageWords> {
        let page = document
            .pages()
            .get(page_index)
            .map_err(|e| page_error(e, page_index))?;
        let (width_pt, height_pt) = (page.width().value, page.height().value);
        let is_a_side = |points: f32| points.is_finite() && points > 0.0;
        if !is_a_side(width_pt) || !is_a_side(height_pt) {
            return Err(ConvertFailure::err(
                FailureCode::PdfParseError,
                format!("page {page_index} declares a size of {width_pt} x {height_pt} points"),
            ));
        }

        // A square of sidecar units as the device: pdfium maps a point on
        // the page to it exactly as it maps it to the pixels of a render.
        let side = i32::from(BOX_UNITS_PER_PAGE_SIDE);
        let to_units = PdfRenderConfig::new()
            .set_target_width(side)
            .set_target_height(side);

        let text = page.text().map_err(|e| page_error(e, page_index))?;
        let characters = text.chars();
        let mut glyphs = Vec::with_capacity(characters.len());
        // By index: the collection's iterator ends at the first character
        // it cannot read instead of reporting it.
        for index in 0..characters.len() {
            let glyph = characters
                .get(index)
                .and_then(|character| read_glyph(&page, &character, &to_units))
                .map_err(|e| text_layer_error(e, page_index, index))?;
            glyphs.push(glyph);
        }

        let mut words = PageWords::new(
            u32::from(page_index) + 1,
            WordOrigin::TextLayer,
            width_pt,
            height_pt,
        );
        for (word, bounds) in group_words(glyphs) {
            words.push(word, bounds);
        }
        Ok(words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Font size of the test glyphs, in points: each box is that tall and
    /// half as wide.
    const SIZE_PT: f32 = 10.0;
    const ADVANCE_PT: f32 = SIZE_PT / 2.0;

    /// A glyph of horizontal text whose box starts at `(x, baseline)`.
    fn glyph(character: char, x: f32, baseline: f32) -> Option<Glyph> {
        Some(Glyph {
            character,
            bounds_pt: [x, baseline, x + ADVANCE_PT, baseline + SIZE_PT],
            origin_pt: [x, baseline],
            angle_rad: 0.0,
            unit_box: [
                x as u16,
                baseline as u16,
                (x + ADVANCE_PT) as u16,
                (baseline + SIZE_PT) as u16,
            ],
        })
    }

    /// `text` set from `x` on one baseline, a space being a separator.
    fn line(text: &str, x: f32, baseline: f32) -> Vec<Option<Glyph>> {
        text.chars()
            .enumerate()
            .map(|(i, character)| {
                if character == ' ' {
                    None
                } else {
                    glyph(character, x + i as f32 * ADVANCE_PT, baseline)
                }
            })
            .collect()
    }

    fn texts(words: &[(String, [u16; 4])]) -> Vec<&str> {
        words.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn whitespace_ends_a_word_and_a_word_box_holds_all_its_characters() {
        let words = group_words(line("the quick fox", 100.0, 700.0));
        assert_eq!(texts(&words), ["the", "quick", "fox"]);
        // "quick" is characters 4..9 of the line.
        assert_eq!(words[1].1, [120, 700, 145, 710]);
    }

    #[test]
    fn a_new_line_starts_a_new_word_without_whitespace_between() {
        let mut page = line("exam-", 100.0, 700.0);
        page.extend(line("ple", 50.0, 688.0));
        assert_eq!(texts(&group_words(page)), ["exam-", "ple"]);
    }

    #[test]
    fn a_superscript_stays_with_its_word() {
        let mut page = line("word", 100.0, 700.0);
        page.push(glyph('2', 120.0, 700.0 + 0.4 * SIZE_PT));
        assert_eq!(texts(&group_words(page)), ["word2"]);
    }

    #[test]
    fn a_wide_gap_separates_what_no_whitespace_does() {
        let mut row = line("12", 100.0, 700.0);
        // The next cell of the row starts a third of a character height
        // past the end of this one: just over the bound.
        let cell_x = 110.0 + (WORD_GAP_FRACTION_OF_CHAR_HEIGHT + 0.05) * SIZE_PT;
        row.extend(line("34", cell_x, 700.0));
        assert_eq!(texts(&group_words(row.clone())), ["12", "34"]);

        // Kerning-sized gaps do not.
        let mut kerned = line("12", 100.0, 700.0);
        kerned.extend(line("34", 110.0 + 0.1 * SIZE_PT, 700.0));
        assert_eq!(texts(&group_words(kerned)), ["1234"]);
    }

    #[test]
    fn text_set_right_to_left_or_turned_on_its_side_is_one_word() {
        // Right to left: each character sits before the previous one.
        let reversed: Vec<_> = (0..4)
            .map(|i| glyph('x', 200.0 - i as f32 * ADVANCE_PT, 700.0))
            .collect();
        assert_eq!(texts(&group_words(reversed)), ["xxxx"]);

        // Turned a quarter turn: the line runs up the page, and each box is
        // as tall as the others are wide.
        let turned: Vec<_> = (0..4)
            .map(|i| {
                let y = 100.0 + i as f32 * ADVANCE_PT;
                Some(Glyph {
                    character: 'y',
                    bounds_pt: [300.0 - SIZE_PT, y, 300.0, y + ADVANCE_PT],
                    origin_pt: [300.0, y],
                    angle_rad: std::f32::consts::FRAC_PI_2,
                    unit_box: [290, y as u16, 300, (y + ADVANCE_PT) as u16],
                })
            })
            .collect();
        let words = group_words(turned);
        assert_eq!(texts(&words), ["yyyy"]);
        assert_eq!(words[0].1, [290, 100, 300, 120]);
    }

    #[test]
    fn control_characters_and_noncharacters_are_in_no_word() {
        for separator in ['\u{2}', '\r', '\n', '\u{a0}', '\u{fffe}', '\u{ffff}'] {
            assert!(separates_words(separator), "{separator:?}");
        }
        for kept in ['a', '-', '\u{fffd}', '\u{ad}', 'é', '中'] {
            assert!(!separates_words(kept), "{kept:?}");
        }
    }

    /// How far a box edge may sit from where the font's metrics put it, in
    /// sidecar units: pdfium rounds a device coordinate to a whole unit.
    const EDGE_TOLERANCE_UNITS: f32 = 3.0;

    fn assert_near(actual: u16, expected_units: f32, what: &str) {
        assert!(
            (f32::from(actual) - expected_units).abs() <= EDGE_TOLERANCE_UNITS,
            "{what}: {actual} is not within {EDGE_TOLERANCE_UNITS} of {expected_units}"
        );
    }

    #[test]
    fn a_text_layer_is_read_word_by_word_with_each_box_on_its_glyphs() {
        use crate::pdf_meta::tests::{pdf_with_text, skip_without_pdfium};
        skip_without_pdfium!("a_text_layer_is_read_word_by_word_with_each_box_on_its_glyphs");

        // 12 pt Helvetica on a 612 x 792 pt page, first baseline at
        // (72, 720), the next one 14 pt lower.
        let pdf = pdf_with_text(&[&["The quick brown fox", "jumps over"]]);
        let words = crate::pdfium::with_parser(|parser| {
            let document = parser.open_bytes(&pdf)?;
            PdfParser::page_words(&document, 0)
        })
        .expect("the page's words");

        assert_eq!(words.page, 1);
        assert_eq!(words.origin, WordOrigin::TextLayer);
        assert_eq!((words.width_pt, words.height_pt), (612.0, 792.0));
        assert_eq!(
            words.words,
            ["The", "quick", "brown", "fox", "jumps", "over"]
        );

        let side = f32::from(BOX_UNITS_PER_PAGE_SIDE);
        let across = |points: f32| points / 612.0 * side;
        let down = |points_from_bottom: f32| (792.0 - points_from_bottom) / 792.0 * side;

        // Adobe's metrics for Helvetica give T, h and e advances of 611,
        // 556 and 556 thousandths of the font size: "The" is 20.676 pt wide.
        let [x0, y0, x1, y1] = words.boxes[0];
        assert_near(x0, across(72.0), "the left edge of The");
        assert_near(x1, across(72.0 + 20.676), "the right edge of The");
        // The box holds the line from above the cap height to under the
        // baseline, and is not as tall as two lines.
        const LEADING_PT: f32 = 14.0;
        let one_leading = LEADING_PT / 792.0 * side;
        assert!(f32::from(y0) < down(720.0 + 8.0), "top {y0}");
        assert!(f32::from(y1) > down(720.0), "bottom {y1}");
        assert!(f32::from(y1 - y0) < 1.5 * one_leading, "height {}", y1 - y0);

        // Words advance along the line; the second line starts under the
        // first, one leading lower.
        assert!(words.boxes[1][0] > x1);
        let [next_x0, next_y0, ..] = words.boxes[4];
        assert_near(next_x0, across(72.0), "the left edge of jumps");
        assert_near(next_y0, f32::from(y0) + one_leading, "the top of jumps");
    }

    #[test]
    fn a_page_without_a_text_layer_has_no_words() {
        use crate::pdf_meta::tests::{pdf_with_pages, skip_without_pdfium};
        skip_without_pdfium!("a_page_without_a_text_layer_has_no_words");

        let pdf = pdf_with_pages(1);
        let words = crate::pdfium::with_parser(|parser| {
            let document = parser.open_bytes(&pdf)?;
            PdfParser::page_words(&document, 0)
        })
        .expect("the page's words");
        assert!(words.words.is_empty() && words.boxes.is_empty());
    }
}
