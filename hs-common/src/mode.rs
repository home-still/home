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

    if std::env::var("FORCE_COLOR").is_ok_and(|v| !v.is_empty()) {
        return OutputMode::Rich;
    }

    if !std::io::stderr().is_terminal() {
        return OutputMode::Pipe;
    }

    if std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) {
        return OutputMode::Plain;
    }

    if std::env::var("TERM").is_ok_and(|v| v == "dumb") {
        return OutputMode::Plain;
    }

    OutputMode::Rich
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_flag_returns_pipe() {
        assert_eq!(detect("auto", true), OutputMode::Pipe);
    }

    #[test]
    fn color_never_returns_plain() {
        assert_eq!(detect("never", false), OutputMode::Plain);
    }

    #[test]
    fn color_always_returns_rich() {
        assert_eq!(detect("always", false), OutputMode::Rich);
    }

    #[test]
    fn force_color_returns_rich() {
        // FORCE_COLOR is checked before the TTY check, so this works in test runners
        std::env::set_var("FORCE_COLOR", "1");
        let result = detect("auto", false);
        std::env::remove_var("FORCE_COLOR");
        assert_eq!(result, OutputMode::Rich);
    }
}
