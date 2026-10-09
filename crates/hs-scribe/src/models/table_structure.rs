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

/// The structure head's `(sequence length, vocabulary size)`, or why this
/// model is not the SLANet-Plus the character dictionary was written for.
/// The decode loop slices `seq_len * vocab` logits and indexes `CHAR_DICT`
/// by the arg-max, so a model with a different vocabulary would otherwise
/// panic (or silently mislabel tokens) on the first table.
fn structure_dims(shape: &[i64]) -> Result<(usize, usize)> {
    let [batch, seq, vocab] = shape else {
        anyhow::bail!(
            "table structure head has rank {} (shape {shape:?}), expected 3",
            shape.len()
        );
    };
    if *batch != 1 || *seq < 1 {
        anyhow::bail!(
            "table structure head has shape {shape:?}, expected [1, seq >= 1, {}]",
            CHAR_DICT.len()
        );
    }
    if *vocab != CHAR_DICT.len() as i64 {
        anyhow::bail!(
            "table structure head has {vocab} classes but the character dictionary has {}; \
             this is not a SLANet-Plus model",
            CHAR_DICT.len()
        );
    }
    Ok((*seq as usize, *vocab as usize))
}

/// The bbox head must be `[1, seq, 8]` (four corner points per step).
fn check_bbox_dims(shape: &[i64], seq_len: usize) -> Result<()> {
    if shape != [1, seq_len as i64, 8] {
        anyhow::bail!("table bbox head has shape {shape:?}, expected [1, {seq_len}, 8]");
    }
    Ok(())
}

/// Declared output shapes, checked at load. A `-1` is a dynamic dimension
/// that can only be checked per inference; a static one that is wrong
/// rejects the model file before it serves a single table.
fn check_declared_outputs(shapes: &[Option<Vec<i64>>]) -> Result<()> {
    let (Some(bbox), Some(structure)) = (shapes.first(), shapes.get(1)) else {
        anyhow::bail!(
            "table model has {} outputs, expected bbox and structure heads",
            shapes.len()
        );
    };
    let bbox = bbox
        .as_ref()
        .context("table model's first output is not a tensor")?;
    let structure = structure
        .as_ref()
        .context("table model's second output is not a tensor")?;
    if bbox.len() != 3 || (bbox[2] != -1 && bbox[2] != 8) {
        anyhow::bail!("table bbox head is declared {bbox:?}, expected [1, seq, 8]");
    }
    if structure.len() != 3 || (structure[2] != -1 && structure[2] != CHAR_DICT.len() as i64) {
        anyhow::bail!(
            "table structure head is declared {structure:?}, expected [1, seq, {}]",
            CHAR_DICT.len()
        );
    }
    Ok(())
}

/// Pixel size the table image is resized to so its longer side is
/// `MAX_LEN`. A sliver whose short side rounds to 0 keeps 1 pixel (an
/// empty image would panic the resize); an empty image is refused.
fn resize_dims(orig_w: u32, orig_h: u32) -> Result<(u32, u32, f32)> {
    if orig_w == 0 || orig_h == 0 {
        anyhow::bail!("table crop is {orig_w}x{orig_h}: nothing to recognize");
    }
    let ratio = MAX_LEN as f32 / (orig_w.max(orig_h) as f32);
    let resize_w = ((orig_w as f32 * ratio) as u32).max(1);
    let resize_h = ((orig_h as f32 * ratio) as u32).max(1);
    Ok((resize_w, resize_h, ratio))
}

impl TableStructureRecognizer {
    pub fn new(model_path: &str, use_cuda: bool) -> Result<Self> {
        let mut builder = Session::builder()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if use_cuda {
            // error_on_failure: ort logs and silently falls back to the CPU
            // provider when CUDA registration fails. `use_cuda: true` is an
            // instruction, not a preference.
            builder = builder
                .with_execution_providers([CUDAExecutionProvider::default()
                    .build()
                    .error_on_failure()])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("Failed to load SLANet-Plus model")?;

        if !session.inputs().iter().any(|i| i.name() == "x") {
            anyhow::bail!("table model has no input named `x`");
        }
        let declared: Vec<Option<Vec<i64>>> = session
            .outputs()
            .iter()
            .map(|o| o.dtype().tensor_shape().map(|s| s.to_vec()))
            .collect();
        check_declared_outputs(&declared).context("table model rejected")?;

        Ok(Self { session })
    }

    pub fn recognize(&mut self, image: &DynamicImage) -> Result<TableStructure> {
        let rgb = image.to_rgb8();
        let (orig_w, orig_h) = (rgb.width(), rgb.height());

        let (resize_w, resize_h, ratio) = resize_dims(orig_w, orig_h)?;

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

        let outputs_len = outputs.len();
        if outputs_len < 2 {
            anyhow::bail!("table model returned {outputs_len} outputs, expected 2");
        }
        let bbox_output = &outputs[0];
        let structure_output = &outputs[1];

        let (bbox_shape, bbox_data) = bbox_output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (struct_shape, struct_data) = structure_output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (seq_len, vocab_size) = structure_dims(struct_shape)?;
        check_bbox_dims(bbox_shape, seq_len)?;

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

            if TD_TOKENS.contains(&token) {
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

/// Append `text` to `html` as element content: `&`, `<` and `>` are escaped
/// so VLM-read cell text ("a<b & c", "<.001") can neither open a tag nor
/// start an entity.
fn push_escaped(html: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            _ => html.push(ch),
        }
    }
}

/// Build HTML table from structure tokens and cell texts. Cell text is
/// escaped; the tokens are the recognizer's own markup and are emitted as is.
pub fn build_html_from_structure(structure: &TableStructure, cell_texts: &[String]) -> String {
    let mut html = String::from("<table>");
    let mut cell_idx = 0;

    for token in &structure.tokens {
        if token == "<td></td>" {
            let text = cell_texts.get(cell_idx).map(|s| s.as_str()).unwrap_or("");
            html.push_str("<td>");
            push_escaped(&mut html, text);
            html.push_str("</td>");
            cell_idx += 1;
        } else if token == "<td" {
            html.push_str("<td");
        } else if token == ">" {
            html.push('>');
            let text = cell_texts.get(cell_idx).map(|s| s.as_str()).unwrap_or("");
            push_escaped(&mut html, text);
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
    fn cell_text_is_escaped_so_it_cannot_become_markup() {
        let structure = TableStructure {
            tokens: vec![
                "<tr>".into(),
                "<td></td>".into(),
                "<td".into(),
                " colspan=\"2\"".into(),
                ">".into(),
                "</td>".into(),
                "</tr>".into(),
            ],
            cells: vec![],
            confidence: 0.9,
        };
        let texts = vec!["a<b & c".into(), "<script>x</script> &amp;".into()];
        let html = build_html_from_structure(&structure, &texts);
        assert_eq!(
            html,
            "<table><tr><td>a&lt;b &amp; c</td><td colspan=\"2\">\
             &lt;script&gt;x&lt;/script&gt; &amp;amp;</td></tr></table>"
        );
    }

    #[test]
    fn test_char_dict_size() {
        assert_eq!(CHAR_DICT.len(), 50);
        assert_eq!(CHAR_DICT[0], "sos");
        assert_eq!(CHAR_DICT[7], "<td");
        assert_eq!(CHAR_DICT[48], "<td></td>");
        assert_eq!(CHAR_DICT[49], "eos");
    }

    #[test]
    fn the_structure_head_must_match_the_character_dictionary() {
        let dict = CHAR_DICT.len() as i64;
        assert_eq!(
            structure_dims(&[1, 488, dict]).unwrap(),
            (488, CHAR_DICT.len())
        );
        // A different model file: another vocabulary, rank or batch.
        for bad in [
            vec![1, 488, dict - 1],
            vec![1, 488, dict + 1],
            vec![1, 488, 0],
            vec![1, 0, dict],
            vec![2, 488, dict],
            vec![1, dict],
            vec![1, 1, 488, dict],
            vec![],
        ] {
            assert!(structure_dims(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_bbox_head_must_line_up_with_the_structure_head() {
        check_bbox_dims(&[1, 488, 8], 488).unwrap();
        for bad in [
            vec![1, 487, 8],
            vec![1, 488, 4],
            vec![1, 488],
            vec![2, 488, 8],
        ] {
            assert!(check_bbox_dims(&bad, 488).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn declared_output_shapes_are_checked_at_load_but_dynamic_dims_pass() {
        let dict = CHAR_DICT.len() as i64;
        let ok = |bbox: Vec<i64>, structure: Vec<i64>| {
            check_declared_outputs(&[Some(bbox), Some(structure)])
        };
        ok(vec![1, 488, 8], vec![1, 488, dict]).unwrap();
        ok(vec![-1, -1, 8], vec![-1, -1, dict]).unwrap();
        ok(vec![-1, -1, -1], vec![-1, -1, -1]).unwrap();
        assert!(ok(vec![1, 488, 8], vec![1, 488, dict + 7]).is_err());
        assert!(ok(vec![1, 488, 4], vec![1, 488, dict]).is_err());
        assert!(ok(vec![1, 488], vec![1, 488, dict]).is_err());
        assert!(check_declared_outputs(&[]).is_err());
        assert!(check_declared_outputs(&[Some(vec![1, 488, 8])]).is_err());
        assert!(check_declared_outputs(&[Some(vec![1, 488, 8]), None]).is_err());
    }

    #[test]
    fn resize_never_produces_a_zero_side_and_refuses_an_empty_image() {
        // 1 x 1000 px: the short side rounds to 0 at ratio 0.488.
        let (w, h, ratio) = resize_dims(1, 1000).unwrap();
        assert_eq!((w, h), (1, 488));
        assert!((ratio - 0.488).abs() < 1e-3);
        let (w, h, _) = resize_dims(2000, 1).unwrap();
        assert_eq!((w, h), (488, 1));
        let (w, h, _) = resize_dims(400, 400).unwrap();
        assert_eq!((w, h), (488, 488));
        assert!(resize_dims(0, 100).is_err());
        assert!(resize_dims(100, 0).is_err());
        assert!(resize_dims(0, 0).is_err());
    }
}
