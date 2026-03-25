use hs_scribe::config::AppConfig;
use hs_scribe::pipeline::processor::Processor;

/// Test that the processor can be constructed with default config.
#[test]
fn test_processor_construction() {
    let config = AppConfig::default();
    let result = Processor::new(config);
    assert!(
        result.is_ok(),
        "Failed to create processor: {:?}",
        result.err()
    );
}
