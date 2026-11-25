//! The word-box sidecar: where every word of a source document sits on
//! its page.
//!
//! A converted markdown says *what* a page says, not *where*. The sidecar
//! holds, for every page of a source PDF, the page's words in reading
//! order and the rectangle each one occupies, so a passage found in the
//! markdown can be drawn on the page it was read from.
//!
//! There is one sidecar per source document. It is stored in the corpus's
//! storage under the store's boxes prefix ([`boxes_key`]) as
//! gzip-compressed JSON, and it names the SHA-256 of the source it was
//! built from, so a sidecar can be told apart from one built for an
//! earlier upload of the same stem.
//!
//! # Coordinates
//!
//! A box is `[x0, y0, x1, y1]` in fractions of the page, not in points or
//! pixels: `x` runs from 0 at the left edge to [`BOX_UNITS_PER_PAGE_SIDE`]
//! at the right edge, `y` from 0 at the **top** edge to
//! [`BOX_UNITS_PER_PAGE_SIDE`] at the bottom edge. A renderer multiplies
//! by its own pixel size, whatever resolution it draws the page at. The
//! page's size in points is kept beside the boxes for its aspect ratio.
//!
//! # One format
//!
//! [`encode`] and [`decode`] both check the sidecar ([`WordBoxes::validate`]),
//! so a malformed one can be neither stored nor read. A sidecar of another
//! [`WORD_BOXES_VERSION`] is an error, not something to adapt to: sidecars
//! are derived from the sources and are rebuilt.

use std::fmt;
use std::io::Read;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::stores::DocumentLayout;

/// The version of the sidecar format this build writes and reads.
pub const WORD_BOXES_VERSION: u32 = 1;

/// The length of a page side in box units: `x` and `y` are fractions of
/// the page's width and height, in steps of one ten-thousandth, measured
/// from the top-left corner.
pub const BOX_UNITS_PER_PAGE_SIDE: u16 = 10_000;

/// The file extension of a sidecar object.
pub const WORD_BOXES_EXTENSION: &str = "words.json.gz";

/// The most JSON a sidecar may decompress to. A sidecar of a
/// thousand-page book is a few tens of megabytes; this bound exists so a
/// corrupt or hostile object cannot expand without limit.
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

/// The length of a SHA-256 digest in hexadecimal characters.
const SHA256_HEX_CHARS: usize = 64;

/// Where a page's words were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordOrigin {
    /// The PDF's own text layer.
    TextLayer,
    /// Optical character recognition of the rendered page: the page had
    /// no usable text layer (a scan).
    Ocr,
}

/// The words of one page and where each one is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageWords {
    /// The page's number, 1-based.
    pub page: u32,
    /// Where the words were read from.
    pub origin: WordOrigin,
    /// The page's width in PDF points.
    pub width_pt: f32,
    /// The page's height in PDF points.
    pub height_pt: f32,
    /// The words, in reading order.
    pub words: Vec<String>,
    /// The box of each word, index-aligned with `words`: `[x0, y0, x1,
    /// y1]` in box units (see the module documentation).
    pub boxes: Vec<[u16; 4]>,
}

impl PageWords {
    /// A page with no words yet.
    pub fn new(page: u32, origin: WordOrigin, width_pt: f32, height_pt: f32) -> Self {
        Self {
            page,
            origin,
            width_pt,
            height_pt,
            words: Vec::new(),
            boxes: Vec::new(),
        }
    }

    /// Append a word and its box, keeping the two lists in step.
    pub fn push(&mut self, word: String, bounds: [u16; 4]) {
        self.words.push(word);
        self.boxes.push(bounds);
    }

    /// What is wrong with this page, if anything.
    fn fault(&self) -> Option<PageFault> {
        let size_is_valid = |side: f32| side.is_finite() && side > 0.0;
        if !size_is_valid(self.width_pt) || !size_is_valid(self.height_pt) {
            return Some(PageFault::Size {
                width_pt: self.width_pt,
                height_pt: self.height_pt,
            });
        }
        if self.words.len() != self.boxes.len() {
            return Some(PageFault::Count {
                words: self.words.len(),
                boxes: self.boxes.len(),
            });
        }
        for (index, (word, bounds)) in self.words.iter().zip(&self.boxes).enumerate() {
            if word.trim().is_empty() {
                return Some(PageFault::EmptyWord { index });
            }
            let [x0, y0, x1, y1] = *bounds;
            if x0 > x1 || y0 > y1 || x1 > BOX_UNITS_PER_PAGE_SIDE || y1 > BOX_UNITS_PER_PAGE_SIDE {
                return Some(PageFault::Box {
                    index,
                    bounds: *bounds,
                });
            }
        }
        None
    }
}

/// The word boxes of one source document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WordBoxes {
    /// The format version: [`WORD_BOXES_VERSION`].
    pub version: u32,
    /// The SHA-256 of the source the boxes were read from, in lower-case
    /// hexadecimal.
    pub source_sha256: String,
    /// Every page of the source, in order, the first one numbered 1. A
    /// page without words is present with empty lists.
    pub pages: Vec<PageWords>,
}

impl WordBoxes {
    /// The page with this 1-based number.
    pub fn page(&self, number: u32) -> Option<&PageWords> {
        let index = usize::try_from(number.checked_sub(1)?).ok()?;
        self.pages.get(index)
    }

    /// Check everything a reader relies on: the version, the source's
    /// hash, the pages numbered 1, 2, 3… without a gap, and on every page
    /// a positive size, one box per word, no blank word and no box that
    /// is inverted or outside the page.
    pub fn validate(&self) -> Result<(), WordBoxesError> {
        if self.version != WORD_BOXES_VERSION {
            return Err(WordBoxesError::Version {
                found: self.version,
            });
        }
        let is_lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if self.source_sha256.len() != SHA256_HEX_CHARS
            || !self.source_sha256.bytes().all(is_lower_hex)
        {
            return Err(WordBoxesError::SourceHash {
                found: self.source_sha256.clone(),
            });
        }
        let mut expected: u32 = 0;
        for page in &self.pages {
            expected = expected.saturating_add(1);
            let fault = if page.page == expected {
                page.fault()
            } else {
                Some(PageFault::Number { expected })
            };
            if let Some(fault) = fault {
                return Err(WordBoxesError::Page {
                    page: page.page,
                    fault,
                });
            }
        }
        Ok(())
    }
}

/// What is wrong with one page of a sidecar.
#[derive(Debug, Clone, PartialEq)]
pub enum PageFault {
    /// The pages are not numbered 1, 2, 3… in order.
    Number {
        /// The number this position should carry.
        expected: u32,
    },
    /// The page's size is not a positive, finite number of points.
    Size {
        /// The width as recorded.
        width_pt: f32,
        /// The height as recorded.
        height_pt: f32,
    },
    /// The page does not have one box per word.
    Count {
        /// How many words the page lists.
        words: usize,
        /// How many boxes it lists.
        boxes: usize,
    },
    /// A word is empty or only whitespace.
    EmptyWord {
        /// The word's index on the page.
        index: usize,
    },
    /// A box is inverted or reaches outside the page.
    Box {
        /// The word's index on the page.
        index: usize,
        /// The box as recorded.
        bounds: [u16; 4],
    },
}

impl fmt::Display for PageFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number { expected } => {
                write!(f, "it is where page {expected} should be")
            }
            Self::Size {
                width_pt,
                height_pt,
            } => write!(
                f,
                "its size is {width_pt} x {height_pt} pt, which is not a page"
            ),
            Self::Count { words, boxes } => {
                write!(f, "it lists {words} word(s) and {boxes} box(es)")
            }
            Self::EmptyWord { index } => write!(f, "word {index} is blank"),
            Self::Box { index, bounds } => write!(
                f,
                "the box of word {index} is {bounds:?}, which is inverted or outside \
                 0..={BOX_UNITS_PER_PAGE_SIDE}"
            ),
        }
    }
}

/// Why a sidecar could not be written or read.
#[derive(Debug)]
pub enum WordBoxesError {
    /// The bytes are not gzip, or the gzip stream is damaged.
    Gzip(std::io::Error),
    /// The JSON does not have the sidecar's shape.
    Json(serde_json::Error),
    /// The object decompresses to more than a sidecar can be.
    TooLarge {
        /// The bound it exceeded.
        limit_bytes: u64,
    },
    /// The sidecar is of a version this build does not read.
    Version {
        /// The version it carries.
        found: u32,
    },
    /// `source_sha256` is not a lower-case hexadecimal SHA-256.
    SourceHash {
        /// The value as recorded.
        found: String,
    },
    /// A page is malformed.
    Page {
        /// The number the page carries.
        page: u32,
        /// What is wrong with it.
        fault: PageFault,
    },
}

impl fmt::Display for WordBoxesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gzip(e) => write!(f, "the word-box sidecar is not readable gzip: {e}"),
            Self::Json(e) => write!(f, "the word-box sidecar is not valid: {e}"),
            Self::TooLarge { limit_bytes } => write!(
                f,
                "the word-box sidecar decompresses to more than {limit_bytes} bytes"
            ),
            Self::Version { found } => write!(
                f,
                "the word-box sidecar is version {found}; this build reads version \
                 {WORD_BOXES_VERSION}. Rebuild the sidecar, or upgrade"
            ),
            Self::SourceHash { found } => write!(
                f,
                "the word-box sidecar's source_sha256 is {found:?}, not a lower-case \
                 hexadecimal SHA-256"
            ),
            Self::Page { page, fault } => {
                write!(
                    f,
                    "page {page} of the word-box sidecar is malformed: {fault}"
                )
            }
        }
    }
}

impl std::error::Error for WordBoxesError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gzip(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::TooLarge { .. }
            | Self::Version { .. }
            | Self::SourceHash { .. }
            | Self::Page { .. } => None,
        }
    }
}

/// The storage key of a stem's sidecar in a store:
/// `<boxes prefix>/<shard>/<stem>.words.json.gz`.
///
/// Does not validate `stem`; call [`crate::validate_stem`] first on
/// untrusted input.
pub fn boxes_key(layout: &DocumentLayout, stem: &str) -> String {
    format!(
        "{}/{}",
        layout.boxes_prefix,
        crate::sharded_key(stem, WORD_BOXES_EXTENSION)
    )
}

/// A rectangle given as fractions of the page (`0.0` at the left and top
/// edges, `1.0` at the right and bottom edges), as a box in sidecar units.
///
/// The two corners may come in either order. A coordinate outside the
/// page is moved to the page's edge: the loose bounds of a glyph can
/// overhang the page by a hair. `None` when a coordinate is not a finite
/// number, which no rectangle on a page has.
pub fn unit_box(x_a: f32, y_a: f32, x_b: f32, y_b: f32) -> Option<[u16; 4]> {
    let units = |fraction: f32| -> Option<u16> {
        if !fraction.is_finite() {
            return None;
        }
        // In 0.0..=10000.0 after the clamp, so the conversion is exact.
        Some((fraction.clamp(0.0, 1.0) * f32::from(BOX_UNITS_PER_PAGE_SIDE)).round() as u16)
    };
    let (x_a, y_a, x_b, y_b) = (units(x_a)?, units(y_a)?, units(x_b)?, units(y_b)?);
    Some([x_a.min(x_b), y_a.min(y_b), x_a.max(x_b), y_a.max(y_b)])
}

/// The sidecar as the bytes to store: gzip-compressed JSON. Refuses a
/// sidecar that [`WordBoxes::validate`] refuses.
pub fn encode(boxes: &WordBoxes) -> Result<Vec<u8>, WordBoxesError> {
    boxes.validate()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    serde_json::to_writer(&mut encoder, boxes).map_err(WordBoxesError::Json)?;
    encoder.finish().map_err(WordBoxesError::Gzip)
}

/// Read a stored sidecar. Refuses bytes that are not a valid sidecar of
/// [`WORD_BOXES_VERSION`].
pub fn decode(bytes: &[u8]) -> Result<WordBoxes, WordBoxesError> {
    decode_within(bytes, MAX_DECODED_BYTES)
}

/// [`decode`] with the decompressed-size bound as a parameter.
fn decode_within(bytes: &[u8], limit_bytes: u64) -> Result<WordBoxes, WordBoxesError> {
    let mut json = Vec::new();
    GzDecoder::new(bytes)
        .take(limit_bytes.saturating_add(1))
        .read_to_end(&mut json)
        .map_err(WordBoxesError::Gzip)?;
    if u64::try_from(json.len()).map_or(true, |len| len > limit_bytes) {
        return Err(WordBoxesError::TooLarge { limit_bytes });
    }
    let boxes: WordBoxes = match serde_json::from_slice(&json) {
        Ok(boxes) => boxes,
        Err(shape) => {
            // A sidecar of another version need not have this version's
            // shape. When it says which version it is, that is the
            // error: it tells the operator what to do.
            #[derive(Deserialize)]
            struct Versioned {
                version: u32,
            }
            return Err(match serde_json::from_slice::<Versioned>(&json) {
                Ok(Versioned { version }) if version != WORD_BOXES_VERSION => {
                    WordBoxesError::Version { found: version }
                }
                _ => WordBoxesError::Json(shape),
            });
        }
    };
    boxes.validate()?;
    Ok(boxes)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::stores::{Stores, ACADEMIC, PERSONAL, PERSONAL_PREFIX_DEFAULT};

    const SHA: &str = "9f86d081884c7d659a2feae0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn sidecar() -> WordBoxes {
        let mut first = PageWords::new(1, WordOrigin::TextLayer, 612.0, 792.0);
        first.push("Attention".to_string(), [1000, 900, 2400, 1100]);
        first.push("is".to_string(), [2450, 900, 2650, 1100]);
        let blank = PageWords::new(2, WordOrigin::TextLayer, 612.0, 792.0);
        let mut scanned = PageWords::new(3, WordOrigin::Ocr, 595.28, 841.89);
        scanned.push("naïve".to_string(), [0, 0, 10_000, 10_000]);
        WordBoxes {
            version: WORD_BOXES_VERSION,
            source_sha256: SHA.to_string(),
            pages: vec![first, blank, scanned],
        }
    }

    fn gzip(json: &str) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_sidecar_survives_the_round_trip() {
        let boxes = sidecar();
        let bytes = encode(&boxes).unwrap();
        assert_eq!(&bytes[..2], [0x1f, 0x8b], "the stored object is gzip");
        assert_eq!(decode(&bytes).unwrap(), boxes);
    }

    #[test]
    fn the_stored_json_has_the_documented_shape() {
        let mut json = String::new();
        GzDecoder::new(encode(&sidecar()).unwrap().as_slice())
            .read_to_string(&mut json)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["source_sha256"], SHA);
        assert_eq!(value["pages"][0]["page"], 1);
        assert_eq!(value["pages"][0]["origin"], "text_layer");
        assert_eq!(value["pages"][2]["origin"], "ocr");
        assert_eq!(
            value["pages"][0]["words"],
            serde_json::json!(["Attention", "is"])
        );
        assert_eq!(
            value["pages"][0]["boxes"][1],
            serde_json::json!([2450, 900, 2650, 1100])
        );
    }

    #[test]
    fn a_page_is_found_by_its_number() {
        let boxes = sidecar();
        assert_eq!(boxes.page(3).unwrap().origin, WordOrigin::Ocr);
        assert!(boxes.page(0).is_none());
        assert!(boxes.page(4).is_none());
    }

    #[test]
    fn another_version_is_refused_by_name_even_when_its_shape_differs() {
        let same_shape = gzip(&format!(
            r#"{{"version":2,"source_sha256":"{SHA}","pages":[]}}"#
        ));
        assert!(matches!(
            decode(&same_shape),
            Err(WordBoxesError::Version { found: 2 })
        ));

        let other_shape = gzip(r#"{"version":7,"lines":[]}"#);
        let err = decode(&other_shape).unwrap_err();
        assert!(matches!(err, WordBoxesError::Version { found: 7 }), "{err}");
        assert!(err.to_string().contains("version 7"), "{err}");

        let mut wrong = sidecar();
        wrong.version = 2;
        assert!(matches!(
            encode(&wrong),
            Err(WordBoxesError::Version { found: 2 })
        ));
    }

    #[test]
    fn a_malformed_sidecar_is_refused_when_written_and_when_read() {
        type Break = fn(&mut WordBoxes);
        let faults: [(&str, Break); 7] = [
            ("a word without a box", |b| {
                b.pages[0].words.push("orphan".to_string())
            }),
            ("a box past the page's edge", |b| {
                b.pages[0].boxes[0] = [0, 0, 10_001, 10]
            }),
            ("an inverted box", |b| {
                b.pages[0].boxes[0] = [500, 0, 400, 10]
            }),
            ("a blank word", |b| b.pages[0].words[1] = " ".to_string()),
            ("a page of no size", |b| b.pages[1].width_pt = 0.0),
            ("a missing page", |b| b.pages[2].page = 4),
            ("a page numbered from zero", |b| b.pages[0].page = 0),
        ];
        for (what, break_it) in faults {
            let mut broken = sidecar();
            break_it(&mut broken);
            assert!(
                matches!(encode(&broken), Err(WordBoxesError::Page { .. })),
                "{what} was written"
            );
            // The same sidecar as a writer that skipped the check would
            // have stored it.
            let stored = gzip(&serde_json::to_string(&broken).unwrap());
            assert!(
                matches!(decode(&stored), Err(WordBoxesError::Page { .. })),
                "{what} was read"
            );
        }
    }

    #[test]
    fn the_source_hash_must_be_a_lower_case_sha256() {
        for bad in ["", "9f86d0", &SHA.to_uppercase(), &SHA.replace('9', "g")] {
            let mut boxes = sidecar();
            boxes.source_sha256 = bad.to_string();
            assert!(
                matches!(encode(&boxes), Err(WordBoxesError::SourceHash { .. })),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn bytes_that_are_not_a_sidecar_are_refused() {
        assert!(matches!(
            decode(b"%PDF-1.7 not gzip"),
            Err(WordBoxesError::Gzip(_))
        ));
        assert!(matches!(
            decode(&gzip("[1, 2, 3]")),
            Err(WordBoxesError::Json(_))
        ));
        // A field this version does not define is a malformed sidecar,
        // not one to read around.
        let extra = gzip(&format!(
            r#"{{"version":1,"source_sha256":"{SHA}","pages":[],"lines":[]}}"#
        ));
        assert!(matches!(decode(&extra), Err(WordBoxesError::Json(_))));
    }

    #[test]
    fn an_object_that_expands_past_the_bound_is_refused() {
        let bytes = encode(&sidecar()).unwrap();
        const TINY_LIMIT_BYTES: u64 = 16;
        assert!(matches!(
            decode_within(&bytes, TINY_LIMIT_BYTES),
            Err(WordBoxesError::TooLarge {
                limit_bytes: TINY_LIMIT_BYTES
            })
        ));
    }

    #[test]
    fn a_sidecar_is_stored_under_its_stores_boxes_prefix() {
        let stores = Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT).unwrap();
        let (_, academic) = stores.documents(ACADEMIC).unwrap();
        assert_eq!(
            boxes_key(academic, "10.1234_abcd"),
            "boxes/10/10.1234_abcd.words.json.gz"
        );
        let (_, personal) = stores.documents(PERSONAL).unwrap();
        assert_eq!(
            boxes_key(personal, "lab-results-abcdef01"),
            "personal/boxes/la/lab-results-abcdef01.words.json.gz"
        );
    }

    #[test]
    fn fractions_of_the_page_become_ordered_units_inside_it() {
        assert_eq!(
            unit_box(0.1, 0.25, 0.5, 0.75),
            Some([1000, 2500, 5000, 7500])
        );
        // Corners in the other order, as a bottom-left origin gives them.
        assert_eq!(
            unit_box(0.5, 0.75, 0.1, 0.25),
            Some([1000, 2500, 5000, 7500])
        );
        // An overhang is moved to the edge.
        assert_eq!(
            unit_box(-0.02, 0.0, 1.01, 1.0),
            Some([0, 0, 10_000, 10_000])
        );
        assert_eq!(unit_box(f32::NAN, 0.0, 0.5, 0.5), None);
        assert_eq!(unit_box(0.0, 0.0, f32::INFINITY, 0.5), None);
    }
}
