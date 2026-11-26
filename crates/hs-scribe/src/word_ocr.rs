//! Reading the words of a page image: how a page with no text layer gets
//! its word boxes.
//!
//! Two PP-OCRv5 models run on ONNX Runtime through `oar-ocr`: one finds the
//! lines of text on the page, the other reads each line and reports where
//! along it each character was read. [`WordOcr::read`] puts the lines in
//! reading order ([`crate::reading_order`]), splits each at its whitespace
//! and gives every word the box of its characters.
//!
//! The models run on the CPU. They are small enough for it, and `oar-ocr`
//! registers its CUDA provider without `error_on_failure`: asking it for
//! the GPU would be a preference ONNX Runtime is free to drop without a
//! word, and an inference path that may silently be the other one is two
//! paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail, Result};
use image::RgbImage;
use oar_ocr::core::config::OrtSessionConfig;
use oar_ocr::domain::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCRBuilder, OAROCR};
use oar_ocr::processors::{BoundingBox, LimitType};

use crate::config::AppConfig;
use crate::reading_order::{reading_order, LineBox};

/// Threads ONNX Runtime may use inside one model call. The server reads one
/// document's pages at a time, so this is the whole CPU cost of the OCR.
/// Measured on a 12-core host, one scanned two-column journal page: 6.5 s
/// with 1 thread, 3.7 s with 2, 2.3 s with 4 and 2.3 s with 8.
const OCR_INTRA_THREADS: usize = 4;

/// Longest side, in pixels, of the image the line detector is given; a
/// larger page image is scaled down to it first. Measured on scanned
/// two-column journal pages rendered at 200 dpi: giving the detector 1600 or
/// the full 2200 pixels found no more of the words and took up to twice as
/// long.
const DETECTION_MAX_SIDE_PX: u32 = 960;

/// Lines the recogniser reads in one call. One: lines read together are
/// padded to the widest of them, and on those same pages reading a page's
/// lines in one call took five times as long as reading them one by one
/// (12 s against 2 s) and got fewer of the words right.
const RECOGNITION_BATCH_LINES: usize = 1;

/// The three files the OCR is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrModels {
    /// The model that finds lines of text (ONNX).
    pub detection: PathBuf,
    /// The model that reads a line (ONNX).
    pub recognition: PathBuf,
    /// The characters the recognition model's outputs stand for, one a line.
    pub dictionary: PathBuf,
}

impl OcrModels {
    /// The files the configuration names.
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            detection: config.resolved_ocr_detection_model_path(),
            recognition: config.resolved_ocr_recognition_model_path(),
            dictionary: config.resolved_ocr_dictionary_path(),
        }
    }

    fn files(&self) -> [(&'static str, &Path); 3] {
        [
            ("text detection model", &self.detection),
            ("text recognition model", &self.recognition),
            ("recognition dictionary", &self.dictionary),
        ]
    }
}

/// One word the OCR read and where it is on the page image.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    /// `[x0, y0, x1, y1]` in pixels of the image read, from its top-left
    /// corner.
    pub bounds_px: [f32; 4],
}

/// The loaded OCR.
pub struct WordOcr {
    engine: OAROCR,
}

impl WordOcr {
    /// Load the models. Fails, naming the file, when one is not installed.
    pub fn load(models: &OcrModels) -> Result<Self> {
        for (role, path) in models.files() {
            if !path.is_file() {
                bail!(
                    "the word OCR's {role} is not at {}: a page without a text layer \
                     cannot be read until it is installed",
                    path.display()
                );
            }
        }
        let detection = TextDetectionConfig {
            limit_side_len: Some(DETECTION_MAX_SIDE_PX),
            limit_type: Some(LimitType::Max),
            ..TextDetectionConfig::default()
        };
        let engine = OAROCRBuilder::new(&models.detection, &models.recognition, &models.dictionary)
            .ort_session(OrtSessionConfig::new().with_intra_threads(OCR_INTRA_THREADS))
            .text_detection_config(detection)
            .region_batch_size(RECOGNITION_BATCH_LINES)
            .return_word_box(true)
            .build()
            .map_err(|e| {
                anyhow!(
                    "loading the word OCR models {} and {}: {e}",
                    models.detection.display(),
                    models.recognition.display()
                )
            })?;
        Ok(Self { engine })
    }

    /// The words on `page`, in reading order.
    pub fn read(&self, page: RgbImage) -> Result<Vec<OcrWord>> {
        let mut pages = self
            .engine
            .predict(vec![page])
            .map_err(|e| anyhow!("reading the page image: {e}"))?;
        let (Some(read), true) = (pages.pop(), pages.is_empty()) else {
            bail!("the OCR did not return exactly one result for one page image");
        };

        let lines = read
            .text_regions
            .iter()
            .filter_map(|region| {
                let text = region.text.as_deref()?;
                Some(OcrLine {
                    text,
                    bounds_px: corners(&region.bounding_box),
                    character_boxes: region.word_boxes.iter().flatten().map(corners).collect(),
                })
            })
            .collect::<Vec<_>>();
        words_in_reading_order(&lines)
    }
}

fn corners(bounds: &BoundingBox) -> [f32; 4] {
    [
        bounds.x_min(),
        bounds.y_min(),
        bounds.x_max(),
        bounds.y_max(),
    ]
}

/// One line as the OCR read it.
struct OcrLine<'a> {
    text: &'a str,
    bounds_px: LineBox,
    /// The box of each character of `text`, in its order.
    character_boxes: Vec<[f32; 4]>,
}

/// The words of `lines`, line by line in the order the page is read.
fn words_in_reading_order(lines: &[OcrLine]) -> Result<Vec<OcrWord>> {
    // A line nothing was read on is no part of the page's layout.
    let read: Vec<&OcrLine> = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .collect();
    let boxes: Vec<LineBox> = read.iter().map(|line| line.bounds_px).collect();
    let mut words = Vec::new();
    for index in reading_order(&boxes) {
        words.extend(line_words(read[index])?);
    }
    Ok(words)
}

/// Split a line at its whitespace; each word's box holds its characters.
fn line_words(line: &OcrLine) -> Result<Vec<OcrWord>> {
    let characters = line.text.chars().count();
    if characters != line.character_boxes.len() {
        bail!(
            "the OCR read {characters} characters on a line and placed {} \
             (is the recognition dictionary the recognition model's own?): {:?}",
            line.character_boxes.len(),
            line.text
        );
    }
    let mut words = Vec::new();
    let mut open: Option<OcrWord> = None;
    for (character, bounds) in line.text.chars().zip(&line.character_boxes) {
        if character.is_whitespace() {
            words.extend(open.take());
            continue;
        }
        match open.as_mut() {
            Some(word) => {
                word.text.push(character);
                word.bounds_px = [
                    word.bounds_px[0].min(bounds[0]),
                    word.bounds_px[1].min(bounds[1]),
                    word.bounds_px[2].max(bounds[2]),
                    word.bounds_px[3].max(bounds[3]),
                ];
            }
            None => {
                open = Some(OcrWord {
                    text: character.to_string(),
                    bounds_px: *bounds,
                });
            }
        }
    }
    words.extend(open);
    Ok(words)
}

/// The OCR of a server, loaded the first time a page needs it: most PDFs
/// have a text layer on every page and never do.
pub struct LazyWordOcr {
    models: OcrModels,
    loaded: Mutex<Option<Arc<WordOcr>>>,
}

impl LazyWordOcr {
    pub fn new(models: OcrModels) -> Self {
        Self {
            models,
            loaded: Mutex::new(None),
        }
    }

    /// The loaded OCR, loading it now if this is its first use. A load that
    /// fails is reported to this caller and tried again by the next one: the
    /// missing file may have been installed since.
    pub fn get(&self) -> Result<Arc<WordOcr>> {
        // The slot holds a finished load or nothing, whatever a panicking
        // holder was doing.
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ocr) = loaded.as_ref() {
            return Ok(Arc::clone(ocr));
        }
        let ocr = Arc::new(WordOcr::load(&self.models)?);
        *loaded = Some(Arc::clone(&ocr));
        Ok(ocr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Width of a test character and height of a test line, in pixels.
    const CHARACTER_PX: f32 = 10.0;
    const LINE_PX: f32 = 20.0;

    /// `text` as the OCR reports a line starting at `(x, y)`.
    fn line(text: &str, x: f32, y: f32) -> (String, LineBox, Vec<[f32; 4]>) {
        let boxes: Vec<[f32; 4]> = (0..text.chars().count())
            .map(|i| {
                let x0 = x + i as f32 * CHARACTER_PX;
                [x0, y, x0 + CHARACTER_PX, y + LINE_PX]
            })
            .collect();
        let width = boxes.len() as f32 * CHARACTER_PX;
        (text.to_string(), [x, y, x + width, y + LINE_PX], boxes)
    }

    fn read(lines: &[(String, LineBox, Vec<[f32; 4]>)]) -> Result<Vec<OcrWord>> {
        let lines: Vec<OcrLine> = lines
            .iter()
            .map(|(text, bounds_px, character_boxes)| OcrLine {
                text,
                bounds_px: *bounds_px,
                character_boxes: character_boxes.clone(),
            })
            .collect();
        words_in_reading_order(&lines)
    }

    #[test]
    fn a_line_is_split_at_its_whitespace_and_each_word_boxes_its_characters() {
        let words = read(&[line("  two  words ", 100.0, 50.0)]).unwrap();
        assert_eq!(
            words,
            [
                OcrWord {
                    text: "two".into(),
                    bounds_px: [120.0, 50.0, 150.0, 70.0],
                },
                OcrWord {
                    text: "words".into(),
                    bounds_px: [170.0, 50.0, 220.0, 70.0],
                },
            ]
        );
    }

    #[test]
    fn lines_are_taken_in_reading_order_not_in_the_order_they_were_found() {
        let words = read(&[
            line("third", 100.0, 200.0),
            line("   ", 100.0, 150.0),
            line("first", 100.0, 100.0),
            line("", 100.0, 300.0),
            line("second", 100.0, 150.0),
        ])
        .unwrap();
        let texts: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[test]
    fn characters_read_but_not_placed_are_an_error_naming_the_line() {
        let (text, bounds, mut boxes) = line("misplaced", 0.0, 0.0);
        boxes.pop();
        let err = read(&[(text, bounds, boxes)]).unwrap_err().to_string();
        assert!(err.contains("read 9 characters"), "{err}");
        assert!(err.contains("placed 8"), "{err}");
        assert!(err.contains("misplaced"), "{err}");
    }

    #[test]
    fn a_missing_model_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("det.onnx");
        std::fs::write(&present, b"not a model").unwrap();
        let models = OcrModels {
            detection: present,
            recognition: dir.path().join("rec.onnx"),
            dictionary: dir.path().join("dict.txt"),
        };
        let ocr = LazyWordOcr::new(models);
        let err = match ocr.get() {
            Ok(_) => panic!("a missing model loaded"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("text recognition model"), "{err}");
        assert!(err.contains("rec.onnx"), "{err}");
    }
}
