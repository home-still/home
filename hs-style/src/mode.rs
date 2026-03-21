use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Rich,
    Plain,
    Pipe,
}

pub fn detect(color_choice: &str, is_json: bool) -> OutputMode {
    if is_json {
        return OutputMode::Pipe;
    }

    match color_choice {
        "never" => return OutputMode::Plain,
        "always" => return OutputMode::Rich,
        _ => {}
    }

    if !std::io::stderr().is_terminal() {
        return OutputMode::Pipe;
    }

    if std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) {
        return OutputMode::Plain;
    }

    if std::env::var("NO_COLOR").is_ok_and(|v| v == "dumb") {
        return OutputMode::Plain;
    }

    OutputMode::Rich
}
