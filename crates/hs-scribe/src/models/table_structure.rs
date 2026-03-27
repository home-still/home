//! SLANet-Plus table structure recognition (ONNX).
//!
//! Single forward pass, 7.4MB model. Returns HTML structure tokens
//! and cell bounding boxes for per-cell OCR.

use anyhow::{Context, Result};
use image::DynamicImage;
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::Session;
use tracing::debug;

const MAX_LEN: u32 = 488;
const IMAGE_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGE_STD: [f32; 3] = [0.229, 0.224, 0.225];
const IMAGE_SCALE: f32 = 1.0 / 255.0;

/// Character dictionary for SLANet-Plus.
/// Order: "sos" + 48 tokens from model metadata + "eos" = 50 total.
const CHAR_DICT: &[&str] = &[
    "sos",             // 0 - BOS
    "<thead>",         // 1
    "</thead>",        // 2
    "<tbody>",         // 3
    "</tbody>",        // 4
    "<tr>",            // 5
    "</tr>",           // 6
    "<td",             // 7  (partial: followed by attributes then ">")
    ">",               // 8
    "</td>",           // 9
    " colspan=\"2\"",  // 10
    " colspan=\"3\"",  // 11
    " colspan=\"4\"",  // 12
    " colspan=\"5\"",  // 13
    " colspan=\"6\"",  // 14
    " colspan=\"7\"",  // 15
    " colspan=\"8\"",  // 16
    " colspan=\"9\"",  // 17
    " colspan=\"10\"", // 18
    " colspan=\"11\"", // 19
    " colspan=\"12\"", // 20
    " colspan=\"13\"", // 21
    " colspan=\"14\"", // 22
    " colspan=\"15\"", // 23
    " colspan=\"16\"", // 24
    " colspan=\"17\"", // 25
    " colspan=\"18\"", // 26
    " colspan=\"19\"", // 27
    " colspan=\"20\"", // 28
    " rowspan=\"2\"",  // 29
    " rowspan=\"3\"",  // 30
    " rowspan=\"4\"",  // 31
    " rowspan=\"5\"",  // 32
    " rowspan=\"6\"",  // 33
    " rowspan=\"7\"",  // 34
    " rowspan=\"8\"",  // 35
    " rowspan=\"9\"",  // 36
    " rowspan=\"10\"", // 37
    " rowspan=\"11\"", // 38
    " rowspan=\"12\"", // 39
    " rowspan=\"13\"", // 40
    " rowspan=\"14\"", // 41
    " rowspan=\"15\"", // 42
    " rowspan=\"16\"", // 43
    " rowspan=\"17\"", // 44
    " rowspan=\"18\"", // 45
    " rowspan=\"19\"", // 46
    " rowspan=\"20\"", // 47
    "<td></td>",       // 48 - standalone empty cell
    "eos",             // 49 - EOS
];

/// Tokens that indicate a cell (and should have an associated bbox).
const TD_TOKENS: &[&str] = &["<td>", "<td", "<td></td>"];

/// A recognized cell with its bounding box relative to the original image.
#[derive(Debug, Clone)]
pub struct TableCell {
    pub bbox: [f32; 4],
}

/// Result of table structure recognition.
#[derive(Debug, Clone)]
pub struct TableStructure {
    pub tokens: Vec<String>,
    pub cells: Vec<TableCell>,
    pub confidence: f32,
}

pub struct TableStructureRecognizer {
    session: Session,
}

impl TableStructureRecognizer {
    pub fn new(model_path: &str, use_cuda: bool) -> Result<Self> {
        let mut builder = Session::builder()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if use_cuda {
            builder = builder
                .with_execution_providers([CUDAExecutionProvider::default().build()])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("Failed to load SLANet-Plus model")?;

        Ok(Self { session })
    }

    pub fn recognize(&mut self, image: &DynamicImage) -> Result<TableStructure> {
        let rgb = image.to_rgb8();
        let (orig_w, orig_h) = (rgb.width(), rgb.height());

        let ratio = MAX_LEN as f32 / (orig_w.max(orig_h) as f32);
        let resize_w = (orig_w as f32 * ratio) as u32;
        let resize_h = (orig_h as f32 * ratio) as u32;

        let resized = image::imageops::resize(
            &rgb,
            resize_w,
            resize_h,
            image::imageops::FilterType::CatmullRom,
        );

        let mut tensor = Array4::<f32>::zeros([1, 3, MAX_LEN as usize, MAX_LEN as usize]);
        for y in 0..resize_h as usize {
            for x in 0..resize_w as usize {
                let pixel = resized.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    tensor[[0, c, y, x]] =
                        (pixel[c] as f32 * IMAGE_SCALE - IMAGE_MEAN[c]) / IMAGE_STD[c];
                }
            }
        }

        let input = ort::value::Value::from_array(tensor).map_err(|e| anyhow::anyhow!("{e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["x" => input])
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let bbox_output = &outputs[0];
        let structure_output = &outputs[1];

        let (_, bbox_data) = bbox_output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (struct_shape, struct_data) = structure_output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let seq_len = struct_shape[1] as usize;
        let vocab_size = struct_shape[2] as usize;

        let eos_idx = CHAR_DICT.len() - 1;

        let mut tokens = Vec::new();
        let mut cells = Vec::new();
        let mut scores = Vec::new();

        let w_ratio = MAX_LEN as f32 / (orig_w as f32 * ratio);
        let h_ratio = MAX_LEN as f32 / (orig_h as f32 * ratio);

        for t in 0..seq_len {
            let offset = t * vocab_size;
            let logits = &struct_data[offset..offset + vocab_size];

            let (char_idx, max_prob) = logits
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .unwrap();

            if t > 0 && char_idx == eos_idx {
                break;
            }
            if char_idx == 0 || char_idx == eos_idx {
                continue;
            }

            let token = CHAR_DICT[char_idx];

            if TD_TOKENS.iter().any(|td| *td == token) {
                let bbox_offset = t * 8;
                if bbox_offset + 7 < bbox_data.len() {
                    let raw_bbox = &bbox_data[bbox_offset..bbox_offset + 8];
                    let xs = [raw_bbox[0], raw_bbox[2], raw_bbox[4], raw_bbox[6]];
                    let ys = [raw_bbox[1], raw_bbox[3], raw_bbox[5], raw_bbox[7]];

                    let min_x = xs.iter().cloned().fold(f32::MAX, f32::min);
                    let max_x = xs.iter().cloned().fold(f32::MIN, f32::max);
                    let min_y = ys.iter().cloned().fold(f32::MAX, f32::min);
                    let max_y = ys.iter().cloned().fold(f32::MIN, f32::max);

                    let x1 = (min_x * orig_w as f32 * w_ratio).max(0.0);
                    let y1 = (min_y * orig_h as f32 * h_ratio).max(0.0);
                    let x2 = (max_x * orig_w as f32 * w_ratio).min(orig_w as f32);
                    let y2 = (max_y * orig_h as f32 * h_ratio).min(orig_h as f32);

                    cells.push(TableCell {
                        bbox: [x1, y1, x2, y2],
                    });
                }
            }

            tokens.push(token.to_string());
            scores.push(*max_prob);
        }

        let confidence = if scores.is_empty() {
            0.0
        } else {
            scores.iter().sum::<f32>() / scores.len() as f32
        };

        debug!(
            "SLANet-Plus: {} tokens, {} cells, confidence={:.3}",
            tokens.len(),
            cells.len(),
            confidence
        );

        Ok(TableStructure {
            tokens,
            cells,
            confidence,
        })
    }
}

/// Build HTML table from structure tokens and cell texts.
pub fn build_html_from_structure(structure: &TableStructure, cell_texts: &[String]) -> String {
    let mut html = String::from("<table>");
    let mut cell_idx = 0;

    for token in &structure.tokens {
        if token == "<td></td>" {
            let text = cell_texts.get(cell_idx).map(|s| s.as_str()).unwrap_or("");
            html.push_str(&format!("<td>{}</td>", text));
            cell_idx += 1;
        } else if token == "<td" {
            html.push_str("<td");
        } else if token == ">" {
            html.push('>');
            let text = cell_texts.get(cell_idx).map(|s| s.as_str()).unwrap_or("");
            html.push_str(text);
            cell_idx += 1;
        } else {
            html.push_str(token);
        }
    }

    html.push_str("</table>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_html_simple() {
        let structure = TableStructure {
            tokens: vec![
                "<thead>".into(),
                "<tr>".into(),
                "<td></td>".into(),
                "<td></td>".into(),
                "</tr>".into(),
                "</thead>".into(),
                "<tbody>".into(),
                "<tr>".into(),
                "<td></td>".into(),
                "<td></td>".into(),
                "</tr>".into(),
                "</tbody>".into(),
            ],
            cells: vec![],
            confidence: 0.95,
        };
        let texts = vec!["Name".into(), "Value".into(), "A".into(), "1".into()];
        let html = build_html_from_structure(&structure, &texts);
        assert!(html.contains("<thead>"));
        assert!(html.contains("<td>Name</td>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn test_build_html_with_colspan() {
        let structure = TableStructure {
            tokens: vec![
                "<tr>".into(),
                "<td".into(),
                " colspan=\"2\"".into(),
                ">".into(),
                "</td>".into(),
                "</tr>".into(),
            ],
            cells: vec![],
            confidence: 0.9,
        };
        let texts = vec!["merged".into()];
        let html = build_html_from_structure(&structure, &texts);
        assert!(html.contains("<td colspan=\"2\">merged</td>"));
    }

    #[test]
    fn test_char_dict_size() {
        assert_eq!(CHAR_DICT.len(), 50);
        assert_eq!(CHAR_DICT[0], "sos");
        assert_eq!(CHAR_DICT[7], "<td");
        assert_eq!(CHAR_DICT[48], "<td></td>");
        assert_eq!(CHAR_DICT[49], "eos");
    }
}
