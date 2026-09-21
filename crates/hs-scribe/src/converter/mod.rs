//! Whole-PDF converters dispatched by `handle_scribe` based on
//! [`crate::config::ConverterMode`]. Each converter takes raw PDF bytes
//! and returns assembled markdown (or an error). The HTTP layer wraps
//! the call in a `tokio::time::timeout` and surfaces the result.
//!
//! The default `Legacy` mode lives inline in `pipeline::processor` and
//! is invoked via the existing `Processor::process_pdf*` methods —
//! handler stays unchanged when `ConverterMode::Legacy` is selected.
//!
//! The `Olmocr` mode lives in [`olmocr_subprocess`] and shells out to
//! the `olmocr` CLI (a Python toolkit that talks to a persistent vLLM
//! serving `allenai/olmOCR-2-7B-1025-FP8`). Olmocr handles render +
//! anchor + prompt + parse + assemble end-to-end — the per-region OCR
//! engine, streaming repetition detector, and QC postprocess are all
//! bypassed in this mode.

pub mod olmocr_subprocess;
