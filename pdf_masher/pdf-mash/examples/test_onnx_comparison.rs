use anyhow::Result;
use image::GenericImageView;
use pdf_mash::models::layout::LayoutDetector;

fn main() -> Result<()> {
    println!("======================================================================");
    println!("DocLayout-YOLO ONNX Inference Test: Rust");
    println!("======================================================================");

    // Load model (CPU only for fair comparison with Python)
    let model_path = "/mnt/datadrive_m2/pdf_masher/models/layout.onnx";
    let mut detector = LayoutDetector::new(model_path, false)?;
    println!("✓ Model loaded: {}", model_path);

    // Load test image
    let image_path = "/mnt/datadrive_m2/pdf_masher/test_document_page.png";
    let image = image::open(image_path)?;
    let (width, height) = image.dimensions();
    println!("✓ Image loaded: {}", image_path);
    println!("  Dimensions: {}×{}", width, height);

    // Run inference
    println!("\n=== Running Inference ===");
    let bboxes = detector.detect(&image)?;

    println!("\n✓ Inference complete");
    println!("  Detected {} bounding boxes", bboxes.len());

    // Print first 2 detections (12 values total)
    println!("\n=== First 2 Detections (Raw Model Output) ===");
    println!("Note: The raw output will be printed by the detector's debug code");
    println!("\nIf you see '[RAW OUTPUT] First 12 values' above, copy those values");
    println!("and compare with Python output.");

    Ok(())
}
