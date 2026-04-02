use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use unicode_width::UnicodeWidthChar;

use crate::reporter::{Reporter, StageHandle};

const DEFAULT_TERM_WIDTH: usize = 80;
const PREFIX_WIDTH_RATIO: usize = 3; // numerator — 60% of terminal width
const PREFIX_WIDTH_DENOM: usize = 5; // denominator
const MIN_PREFIX_WIDTH: usize = 30;
const MAX_PREFIX_WIDTH: usize = 120;
const SPINNER_TICK_MS: u64 = 120;

const PROGRESS_BAR_CHARS: &str = "━╸ ";

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
        let prefix_width = bar_prefix_width();
        let title = truncate_to_width(&sanitize_name(name), prefix_width);

        match total {
            Some(len) => {
                let pb = self.mp.add(ProgressBar::new(len));
                let template = if self.use_color {
                    format!("{{prefix}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                } else {
                    format!("{{prefix}} {{wide_bar}} {{bytes:>10}}/{{total_bytes:<10}} {{msg}}")
                };
                pb.set_style(make_style(&template, PROGRESS_BAR_CHARS));
                let initial = if self.use_color {
                    color_split_prefix(&title, 0.0, prefix_width)
                } else {
                    title.clone()
                };
                pb.set_prefix(initial);
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: false,
                    title,
                })
            }
            None => {
                let pb = self.mp.add(ProgressBar::new_spinner());
                let template = if self.use_color {
                    format!("{{prefix}} {{spinner:.cyan}} {{msg}}")
                } else {
                    format!("{{prefix}} {{spinner}} {{msg}}")
                };
                pb.set_style(make_spinner_style(&template));
                let initial = if self.use_color {
                    color_split_prefix(&title, 0.0, prefix_width)
                } else {
                    title.clone()
                };
                pb.set_prefix(initial);
                pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: false,
                    title,
                })
            }
        }
    }

    fn begin_counted_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle> {
        // Counted stages (e.g. "Downloading", "Converting") use a compact prefix
        // so the wide_bar gets more screen space
        let clean = sanitize_name(name);
        let name_width = display_width(&clean);
        let prefix_width = name_width.max(MIN_PREFIX_WIDTH);
        let title = truncate_to_width(&clean, prefix_width);

        match total {
            Some(len) => {
                let pb = self.mp.add(ProgressBar::new(len));
                let template = if self.use_color {
                    format!("{{prefix}} {{wide_bar:.cyan/dim}} {{pos:>5}}/{{len:<5}} {{elapsed_precise}} ETA {{eta}} {{msg}}")
                } else {
                    format!("{{prefix}} {{wide_bar}} {{pos:>5}}/{{len:<5}} {{elapsed_precise}} ETA {{eta}} {{msg}}")
                };
                pb.set_style(make_style(&template, PROGRESS_BAR_CHARS));
                let initial = if self.use_color {
                    color_split_prefix(&title, 0.0, prefix_width)
                } else {
                    title.clone()
                };
                pb.set_prefix(initial);
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: true,
                    title: title,
                })
            }
            None => {
                let pb = self.mp.add(ProgressBar::new_spinner());
                let template = if self.use_color {
                    format!("{{prefix}} {{spinner:.cyan}} {{msg}}")
                } else {
                    format!("{{prefix}} {{spinner}} {{msg}}")
                };
                pb.set_style(make_spinner_style(&template));
                let initial = if self.use_color {
                    color_split_prefix(&title, 0.0, prefix_width)
                } else {
                    title.clone()
                };
                pb.set_prefix(initial);
                pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
                Box::new(IndicatifStageHandle {
                    pb,
                    use_color: self.use_color,
                    prefix_width,
                    counted: true,
                    title: title,
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
        let template = if self.counted {
            if self.use_color {
                "{prefix} {wide_bar:.cyan/dim} {pos:>5}/{len:<5} {elapsed_precise} ETA {eta} {msg}"
                    .to_string()
            } else {
                "{prefix} {wide_bar} {pos:>5}/{len:<5} {elapsed_precise} ETA {eta} {msg}"
                    .to_string()
            }
        } else if self.use_color {
            "{prefix} {bytes:>10}/{total_bytes:<10} {msg}".to_string()
        } else {
            "{prefix} {wide_bar} {bytes:>10}/{total_bytes:<10} {msg}".to_string()
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
        if self.use_color {
            let red_prefix = color_full_prefix(&self.title, ANSI_BOLD_RED, self.prefix_width);
            self.pb.set_prefix(red_prefix);
            self.pb.set_style(
                ProgressStyle::with_template("{prefix} {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        } else {
            self.pb.set_style(
                ProgressStyle::with_template("{prefix} FAILED: {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        }
        self.pb.finish_with_message(String::from(msg));
    }

    fn finish_skipped(&self, msg: &str) {
        if self.use_color {
            let blue_prefix = color_full_prefix(&self.title, ANSI_BLUE, self.prefix_width);
            self.pb.set_prefix(blue_prefix);
            self.pb.set_style(
                ProgressStyle::with_template("{prefix} {msg:.blue}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        } else {
            self.pb.set_style(
                ProgressStyle::with_template("{prefix} SKIPPED: {msg}")
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
    (term_width * PREFIX_WIDTH_RATIO / PREFIX_WIDTH_DENOM).clamp(MIN_PREFIX_WIDTH, MAX_PREFIX_WIDTH)
}

/// Sanitize control characters in a name (newlines, tabs, etc.) to spaces.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Calculate the display width of a string in terminal columns.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncate a string to fit within `max` display columns, adding ellipsis if needed.
/// Always pads with trailing spaces to exactly `max` columns.
fn truncate_to_width(name: &str, max: usize) -> String {
    let total_width: usize = name.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total_width <= max {
        return pad_to_width(name, max);
    }
    let mut result = String::new();
    let mut width = 0;
    for ch in name.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width + 1 > max {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result.push('\u{2026}'); // …
    pad_to_width(&result, max)
}

/// Pad a string with trailing spaces to reach exactly `target_width` display columns.
/// If the string is already wider, returns it unchanged.
fn pad_to_width(s: &str, target_width: usize) -> String {
    let w = display_width(s);
    if w >= target_width {
        return String::from(s);
    }
    let mut result = String::from(s);
    for _ in 0..(target_width - w) {
        result.push(' ');
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
