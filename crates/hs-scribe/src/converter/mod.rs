//! Whole-PDF converters behind `POST /scribe/stream`, selected by
//! [`crate::config::ConverterMode`]. The server spools the upload to a temp
//! file once; each converter works from that file and returns assembled
//! markdown (or an error). The HTTP layer wraps the call in a
//! `tokio::time::timeout` and surfaces the result.
//!
//! The default `Legacy` mode lives inline in `pipeline::processor`
//! (`Processor::process_pdf_with_progress`) and is the only user of the
//! layout/table ONNX models.
//!
//! The `Olmocr` mode lives in [`olmocr_subprocess`] and shells out to
//! the `olmocr` CLI (a Python toolkit that talks to a persistent vLLM
//! serving `allenai/olmOCR-2-7B-1025-FP8`). Olmocr handles render +
//! anchor + prompt + parse + assemble end-to-end — the per-region OCR
//! engine, streaming repetition detector, and QC postprocess are all
//! bypassed in this mode.

pub mod olmocr_subprocess;
