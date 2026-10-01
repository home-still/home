#![cfg(feature = "server")]

//! Startup contract of the Legacy converter. None of these load a real
//! model or touch a GPU: the cases are "the models are not there" and "the
//! mode that needs no models".

use hs_scribe::config::{AppConfig, PipelineMode};
use hs_scribe::pipeline::processor::Processor;

fn config_in(dir: &std::path::Path) -> AppConfig {
    AppConfig {
        // Never register CUDA from a test.
        use_cuda: false,
        layout_model_path: dir
            .join("no-such-layout.onnx")
            .to_string_lossy()
            .into_owned(),
        table_model_path: dir
            .join("no-such-table.onnx")
            .to_string_lossy()
            .into_owned(),
        ..AppConfig::default()
    }
}

fn start_error(config: AppConfig) -> String {
    match Processor::new(config) {
        Ok(_) => panic!("the processor started although a model it needs is missing"),
        Err(e) => format!("{e:#}"),
    }
}

#[test]
fn per_region_mode_refuses_to_start_without_its_layout_model() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_in(dir.path());
    assert_eq!(cfg.pipeline_mode, PipelineMode::PerRegion);
    let err = start_error(cfg);
    assert!(err.contains("no-such-layout.onnx"), "{err}");
}

#[test]
fn per_region_mode_refuses_to_start_when_the_layout_model_cannot_be_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config_in(dir.path());
    let garbage = dir.path().join("layout.onnx");
    std::fs::write(&garbage, b"this is not an onnx model").unwrap();
    cfg.layout_model_path = garbage.to_string_lossy().into_owned();
    let err = start_error(cfg);
    assert!(err.contains("layout"), "{err}");
}

#[test]
fn full_page_mode_needs_no_models_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = AppConfig {
        pipeline_mode: PipelineMode::FullPage,
        ..config_in(dir.path())
    };
    let processor = Processor::new(cfg).expect("full-page mode loads no models");
    assert!(!processor.has_layout_detector());
    assert!(!processor.has_table_recognizer());
    assert!(processor.layout_model_reason().is_some());
    assert!(processor.table_model_reason().is_some());
}

#[test]
fn a_config_that_would_hang_the_pipeline_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    for broken in [
        AppConfig {
            vlm_concurrency: 0,
            ..config_in(dir.path())
        },
        AppConfig {
            page_parallel: 0,
            ..config_in(dir.path())
        },
        AppConfig {
            region_parallel: 0,
            ..config_in(dir.path())
        },
    ] {
        let err = start_error(broken);
        assert!(err.contains("must be at least 1"), "{err}");
    }
}
