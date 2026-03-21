use pdf_mash::config::AppConfig;
use pdf_mash::pipeline::processor::Processor;

/// Test that the processor can be constructed with default config.
#[test]
fn test_processor_construction() {
    let config = AppConfig::default();
    let result = Processor::new(config);
    assert!(result.is_ok(), "Failed to create processor: {:?}", result.err());
}
