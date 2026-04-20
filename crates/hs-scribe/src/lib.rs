pub mod client;
pub mod config;
pub mod epub;
pub mod event_watch;
pub mod html;
pub mod postprocess;

// Client-side modules (always available)
pub mod cli;

// Server-side modules (heavy deps: ONNX, pdfium, image, etc.)
#[cfg(feature = "server")]
pub mod gpu;
#[cfg(feature = "server")]
pub mod models;
#[cfg(feature = "server")]
pub mod ocr;
#[cfg(feature = "server")]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod utils;
#[cfg(feature = "server")]
pub mod watch;

#[cfg(feature = "eval")]
pub mod eval;
