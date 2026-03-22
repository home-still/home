/// All user-visible output flows through this trait
/// Constructed once in main(), passed as Arc<dyn Reporter>
pub trait Reporter: Send + Sync {
    fn status(&self, verb: &str, message: &str);
    fn warn(&self, message: &str);
    fn error(&self, message: &str);
    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle>;
    fn finish(&self, summary: &str);
}

/// Handle for a single progress stage (spinner or bar)
pub trait StageHandle: Send + Sync {
    fn set_message(&self, msg: &str);
    fn set_length(&self, total: u64); // spinner -> bar transition
    fn set_position(&self, pos: u64);
    fn inc(&self, delta: u64);
    fn finish_with_message(&self, msg: &str);
    fn finish_and_clear(&self);
    fn finish_failed(&self, msg: &str);
}

pub struct SilentReporter;

impl Reporter for SilentReporter {
    fn status(&self, _verb: &str, _message: &str) {}

    fn warn(&self, _message: &str) {}

    fn error(&self, _message: &str) {}

    fn begin_stage(&self, _name: &str, _total: Option<u64>) -> Box<dyn StageHandle> {
        Box::new(NoopStageHandle)
    }

    fn finish(&self, _summary: &str) {}
}

pub struct NoopStageHandle;

impl StageHandle for NoopStageHandle {
    fn set_length(&self, _total: u64) {}
    fn set_message(&self, _msg: &str) {}
    fn set_position(&self, _pos: u64) {}
    fn inc(&self, _delta: u64) {}
    fn finish_with_message(&self, _msg: &str) {}
    fn finish_and_clear(&self) {}
    fn finish_failed(&self, _msg: &str) {}
}
