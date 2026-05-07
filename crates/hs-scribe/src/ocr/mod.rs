mod cloud;
mod engine;
mod ollama;
mod openai_compatible;
pub mod region;
pub mod repetition_detector;
pub mod sse_buffer;

pub use engine::OcrEngine;
pub use region::RegionType;
pub use repetition_detector::{LoopReason, RepetitionDetector, RepetitionLoopError};
