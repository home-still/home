use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

use crate::reporter::{Reporter, StageHandle};

const DEFAULT_TERM_WIDTH: usize = 80;
const PREFIX_WIDTH_RATIO: usize = 2; // numerator of fraction
const PREFIX_WIDTH_DENOM: usize = 5; // denominator — gives 40%
const MIN_PREFIX_WIDTH: usize = 30;
const MAX_PREFIX_WIDTH: usize = 80;
const SPINNER_TICK_MS: u64 = 120;
pub struct TtyReporter {
    mp: MultiProgress,
    use_color: bool,
}

impl TtyReporter {
    pub fn new(use_color: bool) -> Self {
        Self {
            mp: MultiProgress::new(),
            use_color,
        }
    }
}

impl Reporter for TtyReporter {
    fn status(&self, verb: &str, message: &str) {
        if self.use_color {
            let _ = self
                .mp
                .println(format!("{:>12} {}", verb.green().bold(), message));
        } else {
            let _ = self.mp.println(format!("{:>12} {}", verb, message));
        }
    }

    fn warn(&self, message: &str) {
        if self.use_color {
            let _ = self
                .mp
                .println(format!("{}: {}", "warning".yellow().bold(), message));
        } else {
            let _ = self.mp.println(format!("warning: {}", message));
        }
    }

    fn error(&self, message: &str) {
        if self.use_color {
            eprintln!("{}: {}", "error".red().bold(), message);
        } else {
            eprintln!("error: {}", message);
        }
    }

    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle> {
        let pb = match total {
            Some(len) => {
                let pb = self.mp.add(ProgressBar::new(len));
                let prefix_width = bar_prefix_width();
                let template = if self.use_color {
                    format!("{{prefix:{prefix_width}.bold.green}} {{wide_bar:.cyan/dim}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                } else {
                    format!("{{prefix:{prefix_width}}} {{wide_bar}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                };
                pb.set_style(make_style(&template, "━━ "));
                pb.set_prefix(truncate_name(name, prefix_width));
                pb
            }
            None => {
                let pb = self.mp.add(ProgressBar::new_spinner());
                let prefix_width = bar_prefix_width();
                let template = if self.use_color {
                    format!("{{prefix:{prefix_width}.bold.green}} {{spinner:.cyan}} {{msg}}")
                } else {
                    format!("{{prefix:{prefix_width}}} {{spinner}} {{msg}}")
                };
                pb.set_style(make_spinner_style(&template));
                pb.set_prefix(truncate_name(name, bar_prefix_width()));
                pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
                pb
            }
        };

        Box::new(IndicatifStageHandle {
            pb,
            use_color: self.use_color,
            prefix_width: bar_prefix_width(),
        })
    }

    fn finish(&self, summary: &str) {
        if self.use_color {
            let _ = self
                .mp
                .println(format!("{:>12} {}", "Done".green().bold(), summary));
        } else {
            let _ = self.mp.println(format!("{:>12} {}", "Done", summary));
        }
    }
}

struct IndicatifStageHandle {
    pb: ProgressBar,
    use_color: bool,
    prefix_width: usize,
}

impl StageHandle for IndicatifStageHandle {
    fn set_length(&self, total: u64) {
        self.pb.disable_steady_tick();
        self.pb.set_length(total);
        let pw = self.prefix_width;
        let template = if self.use_color {
            format!("{{prefix:{pw}.bold.green}} {{wide_bar:.cyan/dim}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
        } else {
            format!("{{prefix:{pw}}} {{wide_bar}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
        };
        self.pb.set_style(make_style(&template, "━━ "));
    }
    fn set_message(&self, msg: &str) {
        self.pb.set_message(String::from(msg));
    }

    fn set_position(&self, pos: u64) {
        self.pb.set_position(pos);
    }

    fn inc(&self, delta: u64) {
        self.pb.inc(delta);
    }

    fn finish_with_message(&self, msg: &str) {
        self.pb.finish_with_message(String::from(msg));
    }

    fn finish_and_clear(&self) {
        self.pb.finish_and_clear();
    }

    fn finish_failed(&self, msg: &str) {
        let pw = self.prefix_width;
        let term_width = terminal_size::terminal_size()
            .map(|(w, _)| w.0 as usize)
            .unwrap_or(DEFAULT_TERM_WIDTH);
        let max_msg = term_width.saturating_sub(pw + 1);
        let truncated = if msg.len() > max_msg {
            format!("{}...", &msg[..max_msg.saturating_sub(3)])
        } else {
            String::from(msg)
        };

        let template = if self.use_color {
            format!("{{prefix:{pw}.bold.red}} {{msg}}")
        } else {
            format!("{{prefix:{pw}}} FAILED: {{msg}}")
        };
        self.pb.set_style(
            ProgressStyle::with_template(&template)
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        self.pb.finish_with_message(truncated);
    }

    fn finish_skipped(&self, msg: &str) {
        let pw = self.prefix_width;
        let template = if self.use_color {
            format!("{{prefix:{pw}.dim}} {{msg:.dim}}")
        } else {
            format!("{{prefix:{pw}}} SKIPPED: {{msg}}")
        };
        self.pb.set_style(
            ProgressStyle::with_template(&template)
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        self.pb.finish_with_message(String::from(msg));
    }
}

fn make_style(template: &str, progress_chars: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars(progress_chars)
}

fn make_spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

pub fn bar_prefix_width() -> usize {
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(DEFAULT_TERM_WIDTH);
    (term_width * PREFIX_WIDTH_RATIO / PREFIX_WIDTH_DENOM).clamp(MIN_PREFIX_WIDTH, MAX_PREFIX_WIDTH)
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        name.to_string()
    } else {
        format!("{}…", &name[..max - 1])
    }
}
