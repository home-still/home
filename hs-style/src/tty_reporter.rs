use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use unicode_width::UnicodeWidthChar;

use crate::reporter::{Reporter, StageHandle};

const DEFAULT_TERM_WIDTH: usize = 80;
const MIN_PREFIX_WIDTH: usize = 30;
// Reserve space for the right-side info: {bytes:>10}/{total_bytes:<10} {msg}
// 10 + 1 + 10 + 1 + ~10 (msg like "  3.6 MB") + padding = ~35
const RIGHT_SIDE_COLS: usize = 35;
// Compact prefix width for status verbs / spinners (matches status() right-align width)
const SPINNER_PREFIX_WIDTH: usize = 12;
const SPINNER_TICK_MS: u64 = 120;

const PROGRESS_BAR_CHARS: &str = "-> ";

// ANSI escape codes for title-as-progress coloring
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_BOLD_RED: &str = "\x1b[1;31m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_RESET: &str = "\x1b[0m";

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
            let _ = self
                .mp
                .println(format!("{}: {}", "error".red().bold(), message));
        } else {
            let _ = self.mp.println(format!("error: {}", message));
        }
    }

    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle> {
        match total {
            Some(len) => {
                let prefix_width = bar_prefix_width();
                let truncated_title = truncate_name(name, prefix_width);
                let pb = self.mp.add(ProgressBar::new(len));
                let template = if self.use_color {
                    format!("{{prefix:{prefix_width}}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                } else {
                    format!("{{prefix:{prefix_width}}} {{wide_bar}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                };
                pb.set_style(make_style(&template, PROGRESS_BAR_CHARS));
                let initial = if self.use_color {
                    color_split_prefix(&truncated_title, 0.0, prefix_width)
                } else {
                    truncated_title.clone()
                };
                pb.set_prefix(initial);
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: false,
                    title: truncated_title,
                })
            }
            None => {
                let spw = SPINNER_PREFIX_WIDTH;
                let pb = self.mp.add(ProgressBar::new_spinner());
                let template = if self.use_color {
                    format!("{{prefix:>{spw}.bold.green}} {{spinner:.cyan}} {{msg}}")
                } else {
                    format!("{{prefix:>{spw}}} {{spinner}} {{msg}}")
                };
                pb.set_style(make_spinner_style(&template));
                pb.set_prefix(truncate_name(name, spw));
                pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width: bar_prefix_width(),
                    counted: false,
                    title: truncate_name(name, bar_prefix_width()),
                })
            }
        }
    }

    fn begin_counted_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle> {
        match total {
            Some(len) => {
                let prefix_width = bar_prefix_width();
                let truncated_title = truncate_name(name, prefix_width);
                let pb = self.mp.add(ProgressBar::new(len));
                let template = if self.use_color {
                    format!("{{prefix:{prefix_width}}} {{pos:>5}}/{{len:<5}} {{msg}}")
                } else {
                    format!("{{prefix:{prefix_width}}} {{wide_bar}} {{pos:>5}}/{{len:<5}} {{msg}}")
                };
                pb.set_style(make_style(&template, PROGRESS_BAR_CHARS));
                let initial = if self.use_color {
                    color_split_prefix(&truncated_title, 0.0, prefix_width)
                } else {
                    truncated_title.clone()
                };
                pb.set_prefix(initial);
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: true,
                    title: truncated_title,
                })
            }
            None => {
                let spw = SPINNER_PREFIX_WIDTH;
                let pb = self.mp.add(ProgressBar::new_spinner());
                let template = if self.use_color {
                    format!("{{prefix:>{spw}.bold.green}} {{spinner:.cyan}} {{msg}}")
                } else {
                    format!("{{prefix:>{spw}}} {{spinner}} {{msg}}")
                };
                pb.set_style(make_spinner_style(&template));
                pb.set_prefix(truncate_name(name, spw));
                pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width: bar_prefix_width(),
                    counted: true,
                    title: truncate_name(name, bar_prefix_width()),
                })
            }
        }
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
    counted: bool,
    title: String,
}

impl StageHandle for IndicatifStageHandle {
    fn set_length(&self, total: u64) {
        self.pb.disable_steady_tick();
        self.pb.set_length(total);
        let pw = self.prefix_width;
        let template = if self.counted {
            if self.use_color {
                format!("{{prefix:{pw}}} {{pos:>5}}/{{len:<5}} {{msg}}")
            } else {
                format!("{{prefix:{pw}}} {{wide_bar}} {{pos:>5}}/{{len:<5}} {{msg}}")
            }
        } else if self.use_color {
            format!("{{prefix:{pw}}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
        } else {
            format!("{{prefix:{pw}}} {{wide_bar}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
        };
        self.pb.set_style(make_style(&template, PROGRESS_BAR_CHARS));

        // Recalculate prefix coloring at current position
        if self.use_color && total > 0 {
            let frac = self.pb.position() as f64 / total as f64;
            self.pb
                .set_prefix(color_split_prefix(&self.title, frac, self.prefix_width));
        }
    }

    fn set_message(&self, msg: &str) {
        self.pb.set_message(String::from(msg));
    }

    fn set_position(&self, pos: u64) {
        self.pb.set_position(pos);
        if self.use_color {
            if let Some(total) = self.pb.length() {
                if total > 0 {
                    let frac = pos as f64 / total as f64;
                    self.pb
                        .set_prefix(color_split_prefix(&self.title, frac, self.prefix_width));
                }
            }
        }
    }

    fn inc(&self, delta: u64) {
        self.pb.inc(delta);
        if self.use_color {
            if let Some(total) = self.pb.length() {
                if total > 0 {
                    let frac = self.pb.position() as f64 / total as f64;
                    self.pb
                        .set_prefix(color_split_prefix(&self.title, frac, self.prefix_width));
                }
            }
        }
    }

    fn finish_with_message(&self, msg: &str) {
        if self.use_color {
            self.pb
                .set_prefix(color_split_prefix(&self.title, 1.0, self.prefix_width));
        }
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

        if self.use_color {
            let red_prefix = color_full_prefix(&self.title, ANSI_BOLD_RED, pw);
            self.pb.set_prefix(red_prefix);
            let template = format!("{{prefix:{pw}}} {{msg}}");
            self.pb.set_style(
                ProgressStyle::with_template(&template)
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        } else {
            let template = format!("{{prefix:{pw}}} FAILED: {{msg}}");
            self.pb.set_style(
                ProgressStyle::with_template(&template)
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        }
        self.pb.finish_with_message(truncated);
    }

    fn finish_skipped(&self, msg: &str) {
        let pw = self.prefix_width;
        if self.use_color {
            let blue_prefix = color_full_prefix(&self.title, ANSI_BLUE, pw);
            self.pb.set_prefix(blue_prefix);
            let template = format!("{{prefix:{pw}}} {{msg:.blue}}");
            self.pb.set_style(
                ProgressStyle::with_template(&template)
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        } else {
            let template = format!("{{prefix:{pw}}} SKIPPED: {{msg}}");
            self.pb.set_style(
                ProgressStyle::with_template(&template)
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        }
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
    term_width.saturating_sub(RIGHT_SIDE_COLS).max(MIN_PREFIX_WIDTH)
}

/// Truncate a name to fit within `max` display columns, using unicode-aware
/// width measurement. Always pads with trailing spaces to exactly `max` columns.
fn truncate_name(name: &str, max: usize) -> String {
    // Sanitize control characters (newlines, tabs, etc.) to spaces
    let name: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = name.as_str();

    // First pass: check if the name fits as-is
    let total_width: usize = name.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total_width <= max {
        let mut result = String::from(name);
        let mut w = total_width;
        while w < max {
            result.push(' ');
            w += 1;
        }
        return result;
    }

    // Name is too long — truncate and add ellipsis
    let mut result = String::new();
    let mut width = 0;
    let ellipsis_width = 1; // '…' is 1 column

    for ch in name.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width + ellipsis_width > max {
            break;
        }
        result.push(ch);
        width += ch_width;
    }

    result.push('\u{2026}'); // …
    width += ellipsis_width;

    // Pad to exact width (needed if a 2-col char was skipped)
    while width < max {
        result.push(' ');
        width += 1;
    }

    result
}

/// Build a prefix string where the first `fraction` of display columns are
/// bold green and the rest are dim. Used for the title-as-progress-bar effect.
fn color_split_prefix(title: &str, fraction: f64, prefix_width: usize) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    let fill_cols = (prefix_width as f64 * fraction).round() as usize;

    let mut result = String::with_capacity(title.len() + 32); // extra for ANSI codes
    let mut col = 0;
    let mut switched = false;

    // Start with bold green for the filled portion
    result.push_str(ANSI_BOLD_GREEN);

    for ch in title.chars() {
        let ch_width = ch.width().unwrap_or(0);

        // Switch to dim when we've passed the fill point
        if !switched && col + ch_width > fill_cols {
            result.push_str(ANSI_RESET);
            result.push_str(ANSI_DIM);
            switched = true;
        }

        result.push(ch);
        col += ch_width;
    }

    result.push_str(ANSI_RESET);
    result
}

/// Color an entire prefix with a single ANSI style (for failed/skipped states).
fn color_full_prefix(title: &str, ansi_code: &str, _prefix_width: usize) -> String {
    format!("{}{}{}", ansi_code, title, ANSI_RESET)
}
