pub mod backend_probe;
#[cfg(test)]
pub(crate) mod child_proc;
pub mod classify;
pub mod client;
pub mod config;
pub mod diag;
pub mod epub;
pub mod event_watch;
pub mod html;
pub mod pdf_meta;
pub mod pdfium;
pub mod postprocess;
#[cfg(feature = "render")]
pub mod render;
pub mod text_layer;

// Server-side modules (heavy deps: ONNX, image, etc.)
#[cfg(feature = "server")]
pub mod converter;
#[cfg(feature = "server")]
pub mod models;
#[cfg(feature = "server")]
pub mod ocr;
#[cfg(feature = "server")]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod reading_order;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod utils;
#[cfg(feature = "server")]
pub mod word_boxes;
#[cfg(feature = "server")]
pub mod word_ocr;

#[cfg(feature = "eval")]
pub mod eval;
