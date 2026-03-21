use owo_colors::colors::*;
use owo_colors::Style;
use std::default::Default;

pub struct Styles {
    pub verb: Style,
    pub title: Style,
    pub highlight: Style,
    pub doi: Style,
    pub url: Style,
    pub date: Style,
    pub label: Style,
    pub warning: Style,
    pub error_style: Style,
    pub success: Style,
}

impl Styles {
    pub fn colored() -> Self {
        Self {
            verb: Style::new().fg::<Green>().bold(),
            title: Style::new().bold(),
            highlight: Style::new().fg::<Yellow>().bold(),
            doi: Style::new().fg::<Cyan>().underline(),
            url: Style::new().fg::<Cyan>(),
            date: Style::new().dimmed(),
            label: Style::new().fg::<White>(),
            warning: Style::new().fg::<Yellow>(),
            error_style: Style::new().fg::<Red>().bold(),
            success: Style::new().fg::<Green>(),
        }
    }

    pub fn plain() -> Self {
        Self::default()
    }
}

impl Default for Styles {
    fn default() -> Self {
        Self {
            verb: Style::new(),
            title: Style::new(),
            highlight: Style::new(),
            doi: Style::new(),
            url: Style::new(),
            date: Style::new(),
            label: Style::new(),
            warning: Style::new(),
            error_style: Style::new(),
            success: Style::new(),
        }
    }
}
