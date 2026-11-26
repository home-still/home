//! Building a document's word-box sidecar ([`hs_common::word_boxes`]): the
//! words of every page and where each one sits.
//!
//! A page's words come from its text layer ([`PdfParser::page_words`]). A
//! page whose text layer has fewer than [`MIN_TEXT_LAYER_WORDS`] words is a
//! scan: it is rendered and its words are read off the image instead.
//!
//! pdfium serves one caller at a time in the whole process, and the
//! server's page-count probe declares the process wedged when it waits too
//! long for its turn. So the build takes pdfium in short turns of
//! [`PDFIUM_TURN_BUDGET`], and every OCR run happens between two of them.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use hs_common::word_boxes::{
    source_sha256, unit_box, PageWords, WordBoxes, WordOrigin, WORD_BOXES_VERSION,
};
use image::RgbImage;

use crate::pdfium::{guarded_call, with_parser, PdfParser};
use crate::word_ocr::OcrWord;

/// Fewest words a page's text layer must hold to be taken as the page's
/// text. Under it the layer is a stamp or a page number laid over a scan,
/// or nothing at all.
pub const MIN_TEXT_LAYER_WORDS: usize = 5;

/// Resolution a page is rendered at to be read by the OCR.
pub const OCR_RENDER_DPI: u16 = 200;

/// Longest one turn at pdfium lasts before the build gives the other
/// callers theirs. A turn ends after the page that crosses the budget, and
/// always once a page has been rendered.
const PDFIUM_TURN_BUDGET: Duration = Duration::from_secs(2);

/// A page with no text layer, rendered to be read.
struct Scan {
    /// 0-based.
    page_index: u16,
    width_pt: f32,
    height_pt: f32,
    image: RgbImage,
}

/// What one turn at pdfium read: the text layers of consecutive pages and,
/// when the page after them is a scan, that page's image.
struct Turn {
    text_layers: Vec<PageWords>,
    scan: Option<Scan>,
}

/// Build the sidecar of the PDF in `pdf`. `read_image` reads the words of a
/// rendered page; it is only called for a page without a text layer, and
/// never while pdfium is held.
pub fn build_word_boxes(
    pdf: &[u8],
    max_render_pixels: u64,
    read_image: impl Fn(RgbImage) -> Result<Vec<OcrWord>>,
) -> Result<WordBoxes> {
    let counted = with_parser(|parser| guarded_call(|| parser.count_pages_in_bytes(pdf)))?;
    let page_count = u16::try_from(counted)
        .map_err(|_| anyhow!("pdfium counted {counted} pages, more than it can address"))?;

    let mut pages: Vec<PageWords> = Vec::with_capacity(usize::from(page_count));
    while let Some(next) = next_page(pages.len(), page_count) {
        let turn =
            with_parser(|parser| take_turn(parser, pdf, next, page_count, max_render_pixels))?;
        pages.extend(turn.text_layers);
        if let Some(scan) = turn.scan {
            pages.push(read_scan(scan, &read_image)?);
        }
    }

    Ok(WordBoxes {
        version: WORD_BOXES_VERSION,
        source_sha256: source_sha256(pdf),
        pages,
    })
}

/// The index of the first page not read yet, `None` when all are.
fn next_page(pages_read: usize, page_count: u16) -> Option<u16> {
    u16::try_from(pages_read)
        .ok()
        .filter(|next| *next < page_count)
}

/// Read pages from `first` on until the turn's budget is spent, the
/// document ends or a page has to be read by the OCR.
fn take_turn(
    parser: &PdfParser,
    pdf: &[u8],
    first: u16,
    page_count: u16,
    max_render_pixels: u64,
) -> Result<Turn> {
    let started = Instant::now();
    let document = guarded_call(|| parser.open_bytes(pdf))?;
    let mut text_layers = Vec::new();
    for page_index in first..page_count {
        let words = guarded_call(|| PdfParser::page_words(&document, page_index))?;
        if words.words.len() < MIN_TEXT_LAYER_WORDS {
            let rendered = guarded_call(|| {
                PdfParser::render_page(&document, page_index, OCR_RENDER_DPI, max_render_pixels)
            })?;
            return Ok(Turn {
                text_layers,
                scan: Some(Scan {
                    page_index,
                    width_pt: rendered.width,
                    height_pt: rendered.height,
                    image: rendered.image.into_rgb8(),
                }),
            });
        }
        text_layers.push(words);
        if started.elapsed() >= PDFIUM_TURN_BUDGET {
            break;
        }
    }
    Ok(Turn {
        text_layers,
        scan: None,
    })
}

/// The words of a scanned page, each box converted from the pixels of the
/// image it was read on to sidecar units.
fn read_scan(
    scan: Scan,
    read_image: impl Fn(RgbImage) -> Result<Vec<OcrWord>>,
) -> Result<PageWords> {
    let Scan {
        page_index,
        width_pt,
        height_pt,
        image,
    } = scan;
    let (width_px, height_px) = (image.width() as f32, image.height() as f32);
    let read = read_image(image)
        .map_err(|e| e.context(format!("reading page {page_index} with the OCR")))?;

    let mut words = PageWords::new(
        u32::from(page_index) + 1,
        WordOrigin::Ocr,
        width_pt,
        height_pt,
    );
    for OcrWord { text, bounds_px } in read {
        let [x0, y0, x1, y1] = bounds_px;
        let bounds = unit_box(x0 / width_px, y0 / height_px, x1 / width_px, y1 / height_px)
            .ok_or_else(|| {
                anyhow!("page {page_index}: the OCR placed {text:?} at {bounds_px:?}, which is not a box")
            })?;
        words.push(text, bounds);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_RENDER_PIXELS;
    use crate::pdf_meta::tests::{pdf_with_pages, pdf_with_text, skip_without_pdfium};
    use std::cell::RefCell;

    /// The OCR of a document that must not need one.
    fn no_ocr(_: RgbImage) -> Result<Vec<OcrWord>> {
        anyhow::bail!("a page with a text layer was sent to the OCR")
    }

    /// An OCR that reads one word in the lower right quarter of every page.
    fn one_word(image: RgbImage) -> Result<Vec<OcrWord>> {
        let (width, height) = (image.width() as f32, image.height() as f32);
        Ok(vec![OcrWord {
            text: "scanned".into(),
            bounds_px: [width / 2.0, height / 2.0, width, height],
        }])
    }

    #[test]
    fn a_text_layer_is_the_page_s_words_and_the_ocr_is_not_run() {
        skip_without_pdfium!("a_text_layer_is_the_page_s_words_and_the_ocr_is_not_run");
        let pdf = pdf_with_text(&[
            &["one two three four", "five six"],
            &["the second page has five words"],
        ]);
        let boxes = build_word_boxes(&pdf, DEFAULT_MAX_RENDER_PIXELS, no_ocr).expect("build");

        assert_eq!(boxes.source_sha256, source_sha256(&pdf));
        assert_eq!(boxes.pages.len(), 2);
        assert_eq!(
            boxes.pages[0].words,
            ["one", "two", "three", "four", "five", "six"]
        );
        assert_eq!(
            boxes.pages[1].words,
            ["the", "second", "page", "has", "five", "words"]
        );
        for (index, page) in boxes.pages.iter().enumerate() {
            assert_eq!(page.page as usize, index + 1);
            assert_eq!(page.origin, WordOrigin::TextLayer);
        }
        boxes.validate().expect("a built sidecar is a valid one");
    }

    #[test]
    fn only_the_pages_without_enough_of_a_text_layer_are_read_by_the_ocr() {
        skip_without_pdfium!("only_the_pages_without_enough_of_a_text_layer_are_read_by_the_ocr");
        let pdf = pdf_with_text(&[
            &["a first page with five words"],
            &[],
            // A stamp over a scan: under MIN_TEXT_LAYER_WORDS.
            &["Part II"],
            &["a last page with five words"],
        ]);
        let runs = RefCell::new(0);
        let boxes = build_word_boxes(&pdf, DEFAULT_MAX_RENDER_PIXELS, |image| {
            *runs.borrow_mut() += 1;
            one_word(image)
        })
        .expect("build");

        assert_eq!(*runs.borrow(), 2, "one OCR run for each scanned page");
        let origins: Vec<_> = boxes.pages.iter().map(|page| page.origin).collect();
        assert_eq!(
            origins,
            [
                WordOrigin::TextLayer,
                WordOrigin::Ocr,
                WordOrigin::Ocr,
                WordOrigin::TextLayer
            ]
        );
        let numbers: Vec<_> = boxes.pages.iter().map(|page| page.page).collect();
        assert_eq!(numbers, [1, 2, 3, 4]);
        for scanned in [&boxes.pages[1], &boxes.pages[2]] {
            assert_eq!(scanned.words, ["scanned"]);
            assert_eq!(scanned.boxes, [[5_000, 5_000, 10_000, 10_000]]);
            assert_eq!((scanned.width_pt, scanned.height_pt), (612.0, 792.0));
        }
        boxes.validate().expect("a built sidecar is a valid one");
    }

    #[test]
    fn a_page_the_ocr_reads_nothing_on_is_a_page_without_words() {
        skip_without_pdfium!("a_page_the_ocr_reads_nothing_on_is_a_page_without_words");
        let pdf = pdf_with_pages(2);
        let boxes =
            build_word_boxes(&pdf, DEFAULT_MAX_RENDER_PIXELS, |_| Ok(Vec::new())).expect("build");
        assert_eq!(boxes.pages.len(), 2);
        assert!(boxes.pages.iter().all(|page| page.words.is_empty()));
        boxes.validate().expect("a built sidecar is a valid one");
    }

    #[test]
    fn a_failed_ocr_run_fails_the_build_and_names_the_page() {
        skip_without_pdfium!("a_failed_ocr_run_fails_the_build_and_names_the_page");
        let pdf = pdf_with_pages(2);
        let runs = RefCell::new(0);
        let err = build_word_boxes(&pdf, DEFAULT_MAX_RENDER_PIXELS, |_| {
            *runs.borrow_mut() += 1;
            if *runs.borrow() == 2 {
                anyhow::bail!("the model is gone");
            }
            Ok(Vec::new())
        })
        .expect_err("the second page cannot be read");
        let message = format!("{err:#}");
        assert!(message.contains("page 1"), "{message}");
        assert!(message.contains("the model is gone"), "{message}");
    }

    #[test]
    fn a_box_the_ocr_did_not_place_fails_the_build() {
        skip_without_pdfium!("a_box_the_ocr_did_not_place_fails_the_build");
        let pdf = pdf_with_pages(1);
        let err = build_word_boxes(&pdf, DEFAULT_MAX_RENDER_PIXELS, |_| {
            Ok(vec![OcrWord {
                text: "nowhere".into(),
                bounds_px: [f32::NAN, 0.0, 1.0, 1.0],
            }])
        })
        .expect_err("a box with no position");
        assert!(format!("{err:#}").contains("nowhere"), "{err:#}");
    }

    #[test]
    fn bytes_that_are_not_a_pdf_are_a_verdict_on_the_document() {
        skip_without_pdfium!("bytes_that_are_not_a_pdf_are_a_verdict_on_the_document");
        let err = build_word_boxes(
            b"%PDF-1.7\nnot a document",
            DEFAULT_MAX_RENDER_PIXELS,
            no_ocr,
        )
        .expect_err("no document in these bytes");
        assert_eq!(
            crate::classify::failure_code(&err),
            Some(crate::classify::FailureCode::PdfParseError),
            "{err:#}"
        );
    }

    #[test]
    fn the_next_page_is_the_first_one_not_read_until_none_is_left() {
        assert_eq!(next_page(0, 3), Some(0));
        assert_eq!(next_page(2, 3), Some(2));
        assert_eq!(next_page(3, 3), None);
        assert_eq!(next_page(usize::from(u16::MAX), u16::MAX), None);
    }
}
