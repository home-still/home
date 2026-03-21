  use ort::execution_providers::CUDAExecutionProvider;

  fn main() {
      println!("Testing CUDA availability...");

      match CUDAExecutionProvider::default().build() {
          Ok(provider) => println!("✓ CUDA provider created successfully: {:?}",
  provider),
          Err(e) => println!("✗ CUDA provider failed: {}", e),
      }

      println!("\nAvailable execution providers:");
      println!("{:?}", ort::execution_providers::available_execution_providers());
  }
