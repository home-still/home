use anyhow::Result;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::Session;

fn main() -> Result<()> {
    println!("Testing CUDA availability...");

    // Just try to create a CUDA provider
    let provider = CUDAExecutionProvider::default().build();
    println!("CUDA provider: {:?}", provider);

    // Try to create a simple session with CUDA
    let session = Session::builder()?
        .with_execution_providers([CUDAExecutionProvider::default().build()])?
        .commit_from_file("models/paddle_ocr_det.onnx");

    match session {
        Ok(_) => println!("✓ Successfully created ONNX session with CUDA!"),
        Err(e) => println!("✗ Failed to create session with CUDA: {}", e),
    }

    Ok(())
}
