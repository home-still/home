use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Rich,
    Plain,
    Pipe,
}

pub fn detect(color_choice: &str, is_json: bool) -> OutputMode {
    detect_with(
        color_choice,
        is_json,
        |k| std::env::var(k).ok(),
        || std::io::stderr().is_terminal(),
    )
}

/// [`detect`] with the environment lookup and the TTY probe injected.
fn detect_with(
    color_choice: &str,
    is_json: bool,
    env: impl Fn(&str) -> Option<String>,
    stderr_is_terminal: impl Fn() -> bool,
) -> OutputMode {
    if is_json {
        return OutputMode::Pipe;
    }

    match color_choice {
        "never" => return OutputMode::Plain,
        "always" => return OutputMode::Rich,
        _ => {}
    }

    if env("FORCE_COLOR").is_some_and(|v| !v.is_empty()) {
        return OutputMode::Rich;
    }

    if !stderr_is_terminal() {
        return OutputMode::Pipe;
    }

    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return OutputMode::Plain;
    }

    if env("TERM").is_some_and(|v| v == "dumb") {
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
        // FORCE_COLOR is checked before the TTY check.
        let env = |k: &str| (k == "FORCE_COLOR").then(|| "1".to_string());
        assert_eq!(detect_with("auto", false, env, || false), OutputMode::Rich);
    }

    #[test]
    fn empty_force_color_is_ignored() {
        let env = |k: &str| (k == "FORCE_COLOR").then(String::new);
        assert_eq!(detect_with("auto", false, env, || false), OutputMode::Pipe);
    }

    #[test]
    fn terminal_honours_no_color_and_dumb_term() {
        let no_color = |k: &str| (k == "NO_COLOR").then(|| "1".to_string());
        assert_eq!(
            detect_with("auto", false, no_color, || true),
            OutputMode::Plain
        );
        let dumb = |k: &str| (k == "TERM").then(|| "dumb".to_string());
        assert_eq!(detect_with("auto", false, dumb, || true), OutputMode::Plain);
        assert_eq!(
            detect_with("auto", false, |_| None, || true),
            OutputMode::Rich
        );
    }
}
