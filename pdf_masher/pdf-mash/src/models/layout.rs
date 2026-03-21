use anyhow::Result;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;

#[derive(Debug, Clone)]
pub struct BBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub confidence: f32,
    pub class_id: usize,
    pub class_name: String,
    pub unique_id: usize,
    pub read_order: f32,
}

impl BBox {
    pub fn center(&self) -> (f32, f32) {
        ((self.x1 + self.x2) / 2.0, (self.y1 + self.y2) / 2.0)
    }

    pub fn width(&self) -> f32 {
        self.x2 - self.x1
    }

    pub fn height(&self) -> f32 {
        self.y2 - self.y1
    }

    pub fn area(&self) -> f32 {
        self.width() * self.height()
    }

    pub fn overlaps(&self, other: &BBox) -> bool {
        !(self.x2 < other.x1 || self.x1 > other.x2 || self.y2 < other.y1 || self.y1 > other.y2)
    }

    pub fn iou(&self, other: &BBox) -> f32 {
        let x_overlap = (self.x2.min(other.x2) - self.x1.max(other.x1)).max(0.0);
        let y_overlap = (self.y2.min(other.y2) - self.y1.max(other.y1)).max(0.0);

        let intersection = x_overlap * y_overlap;
        let union = self.area() + other.area() - intersection;
        if union > 0.0 {
            intersection / union
        } else {
            0.0
        }
    }

}

/// PP-DocLayout-V3 class names (25 classes, from Paddle config.json label_list)
const CLASS_NAMES: [&str; 25] = [
    "abstract",          // 0
    "algorithm",         // 1
    "aside_text",        // 2
    "chart",             // 3
    "content",           // 4
    "display_formula",   // 5
    "doc_title",         // 6
    "figure_title",      // 7
    "footer",            // 8
    "footer_image",      // 9
    "footnote",          // 10
    "formula_number",    // 11
    "header",            // 12
    "header_image",      // 13
    "image",             // 14
    "inline_formula",    // 15
    "number",            // 16
    "paragraph_title",   // 17
    "reference",         // 18
    "reference_content", // 19
    "seal",              // 20
    "table",             // 21
    "text",              // 22
    "vertical_text",     // 23
    "vision_footnote",   // 24
];

pub struct LayoutDetector {
    session: Session,
    confidence_threshold: f32,
    image_input_name: String,
    im_shape_input_name: String,
    scale_input_name: String,
}

impl LayoutDetector {
    pub fn new(model_path: &str, use_cuda: bool) -> Result<Self> {
        let mut builder = Session::builder()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if use_cuda {
            builder = builder
                .with_execution_providers([CUDAExecutionProvider::default().build()])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Read input names dynamically from the model
        let inputs: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();

        // Expected: im_shape, image, scale_factor (order may vary)
        let im_shape_input_name = inputs
            .iter()
            .find(|n: &&String| n.contains("im_shape"))
            .cloned()
            .unwrap_or_else(|| inputs[0].clone());
        let image_input_name = inputs
            .iter()
            .find(|n: &&String| n.contains("image"))
            .cloned()
            .unwrap_or_else(|| inputs[1].clone());
        let scale_input_name = inputs
            .iter()
            .find(|n: &&String| n.contains("scale"))
            .cloned()
            .unwrap_or_else(|| inputs[2].clone());

        tracing::info!(
            "PP-DocLayout-V3 inputs: im_shape={}, image={}, scale={}",
            im_shape_input_name,
            image_input_name,
            scale_input_name,
        );

        Ok(Self {
            session,
            confidence_threshold: 0.25,
            image_input_name,
            im_shape_input_name,
            scale_input_name,
        })
    }

    pub fn detect(&mut self, image: &DynamicImage) -> Result<Vec<BBox>> {
        let (orig_w, orig_h) = image.dimensions();

        // Preprocessing: direct resize to 800×800, /255.0 only (no ImageNet norm, no letterbox)
        let resized = image.resize_exact(800, 800, FilterType::Triangle);
        let rgb = resized.to_rgb8();

        let mut array = ndarray::Array4::<f32>::zeros((1, 3, 800, 800));
        for y in 0..800u32 {
            for x in 0..800u32 {
                let pixel = rgb.get_pixel(x, y);
                array[[0, 0, y as usize, x as usize]] = pixel[0] as f32 / 255.0;
                array[[0, 1, y as usize, x as usize]] = pixel[1] as f32 / 255.0;
                array[[0, 2, y as usize, x as usize]] = pixel[2] as f32 / 255.0;
            }
        }

        let im_shape = ndarray::Array2::<f32>::from_shape_vec(
            (1, 2),
            vec![800.0, 800.0],
        )?;
        let scale_factor = ndarray::Array2::<f32>::from_shape_vec(
            (1, 2),
            vec![800.0 / orig_h as f32, 800.0 / orig_w as f32],
        )?;

        let image_val =
            ort::value::Value::from_array(array).map_err(|e| anyhow::anyhow!("{e}"))?;
        let im_shape_val =
            ort::value::Value::from_array(im_shape).map_err(|e| anyhow::anyhow!("{e}"))?;
        let scale_val =
            ort::value::Value::from_array(scale_factor).map_err(|e| anyhow::anyhow!("{e}"))?;

        let outputs = self
            .session
            .run(ort::inputs![
                self.im_shape_input_name.as_str() => im_shape_val,
                self.image_input_name.as_str() => image_val,
                self.scale_input_name.as_str() => scale_val
            ])
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Output 0: (N, 7) = [class_id, score, x1, y1, x2, y2, read_order]
        let (det_shape, det_data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let num_dets = det_shape[0] as usize;

        let mut bboxes = Vec::new();

        for i in 0..num_dets {
            let base = i * 7;
            if base + 6 >= det_data.len() {
                break;
            }

            let class_id = det_data[base] as usize;
            let score = det_data[base + 1];
            let x1 = det_data[base + 2];
            let y1 = det_data[base + 3];
            let x2 = det_data[base + 4];
            let y2 = det_data[base + 5];
            let read_order = det_data[base + 6];

            if score < self.confidence_threshold {
                continue;
            }

            let class_name = if class_id < CLASS_NAMES.len() {
                CLASS_NAMES[class_id].to_string()
            } else {
                format!("unknown_{}", class_id)
            };

            // Clamp to image bounds (coords are already in original space)
            let x1 = x1.max(0.0).min(orig_w as f32);
            let y1 = y1.max(0.0).min(orig_h as f32);
            let x2 = x2.max(0.0).min(orig_w as f32);
            let y2 = y2.max(0.0).min(orig_h as f32);

            bboxes.push(BBox {
                x1,
                y1,
                x2,
                y2,
                confidence: score,
                class_id,
                class_name,
                unique_id: bboxes.len(),
                read_order,
            });
        }

        // Sort by native read_order (ascending) — replaces XY-Cut++
        bboxes.sort_by(|a, b| {
            a.read_order
                .partial_cmp(&b.read_order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Reassign unique_ids to match sorted order
        for (i, bbox) in bboxes.iter_mut().enumerate() {
            bbox.unique_id = i;
        }

        Ok(bboxes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bbox(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox {
            x1,
            y1,
            x2,
            y2,
            confidence: 0.9,
            class_id: 0,
            class_name: "text".to_string(),
            unique_id: 0,
            read_order: 0.0,
        }
    }

    #[test]
    fn test_bbox_area() {
        assert!((make_bbox(0.0, 0.0, 10.0, 20.0).area() - 200.0).abs() < f32::EPSILON);
        assert!((make_bbox(5.0, 5.0, 5.0, 10.0).area() - 0.0).abs() < f32::EPSILON); // zero-width
        assert!((make_bbox(5.0, 5.0, 10.0, 5.0).area() - 0.0).abs() < f32::EPSILON); // zero-height
    }

    #[test]
    fn test_bbox_center() {
        let b = make_bbox(10.0, 20.0, 30.0, 40.0);
        assert_eq!(b.center(), (20.0, 30.0));
    }

    #[test]
    fn test_bbox_overlaps() {
        let a = make_bbox(0.0, 0.0, 10.0, 10.0);
        assert!(a.overlaps(&make_bbox(5.0, 5.0, 15.0, 15.0))); // overlapping
        assert!(!a.overlaps(&make_bbox(20.0, 20.0, 30.0, 30.0))); // non-overlapping
        assert!(a.overlaps(&make_bbox(10.0, 0.0, 20.0, 10.0))); // touching edge
    }

    #[test]
    fn test_bbox_iou() {
        let a = make_bbox(0.0, 0.0, 10.0, 10.0);
        // Identical → 1.0
        assert!((a.iou(&make_bbox(0.0, 0.0, 10.0, 10.0)) - 1.0).abs() < f32::EPSILON);
        // No overlap → 0.0
        assert!((a.iou(&make_bbox(20.0, 20.0, 30.0, 30.0)) - 0.0).abs() < f32::EPSILON);
        // Partial overlap: intersection 5×5=25, union 100+100-25=175
        assert!((a.iou(&make_bbox(5.0, 5.0, 15.0, 15.0)) - 25.0 / 175.0).abs() < 1e-5);
        // Zero-area box
        assert!((make_bbox(0.0, 0.0, 0.0, 0.0).iou(&a) - 0.0).abs() < f32::EPSILON);
    }
}
