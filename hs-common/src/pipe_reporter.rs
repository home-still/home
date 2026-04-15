use crate::reporter::{NoopStageHandle, Reporter, StageHandle};

pub struct PipeReporter;

impl Reporter for PipeReporter {
    fn status(&self, verb: &str, message: &str) {
        eprintln!("{}: {}", verb, message);
    }

    fn warn(&self, message: &str) {
        eprintln!("warning: {}", message);
    }

    fn error(&self, message: &str) {
        eprintln!("error: {}", message);
    }

    fn begin_stage(&self, _name: &str, _total: Option<u64>) -> Box<dyn StageHandle> {
        Box::new(NoopStageHandle)
    }

    fn finish(&self, summary: &str) {
        eprintln!("{}", summary);
    }
}
