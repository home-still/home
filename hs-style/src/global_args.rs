use std::path::PathBuf;

#[derive(clap::ValueEnum, Clone, Debug, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(clap::ValueEnum, Clone, Debug, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Ndjson,
}

#[derive(clap::Args, Clone, Debug)]
pub struct GlobalArgs {
    /// Color output: auto|always|never
    #[arg(
        long,
        global = true,
        default_value = "auto",
        help_heading = "Global Options"
    )]
    pub color: ColorChoice,

    /// Output format: text|json|ndjson
    #[arg(
        long,
        global = true,
        default_value = "text",
        help_heading = "Global Options"
    )]
    pub output: OutputFormat,

    /// Suppress all non-result output
    #[arg(long, global = true, help_heading = "Global Options")]
    pub quiet: bool,

    /// Show debug-level output
    #[arg(long, global = true, help_heading = "Global Options")]
    pub verbose: bool,

    /// Override config directory
    #[arg(long, global = true, help_heading = "Global Options")]
    pub config_dir: Option<PathBuf>,

    /// Skip interactive prompts (assume yes)                               
    #[arg(short = 'y', long, global = true, help_heading = "Global Options")]
    pub yes: bool,
}

impl GlobalArgs {
    pub fn is_json(&self) -> bool {
        matches!(self.output, OutputFormat::Json)
    }

    pub fn color_str(&self) -> &str {
        match self.color {
            ColorChoice::Auto => "auto",
            ColorChoice::Always => "always",
            ColorChoice::Never => "never",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        global: GlobalArgs,
    }

    #[test]
    fn defaults_parse() {
        let cli = TestCli::try_parse_from(["test"]).unwrap();
        assert!(matches!(cli.global.color, ColorChoice::Auto));
        assert!(!cli.global.quiet);
        assert!(!cli.global.yes);
        assert!(!cli.global.verbose);
        assert!(!cli.global.is_json());
    }

    #[test]
    fn yes_short_flag() {
        let cli = TestCli::try_parse_from(["test", "-y"]).unwrap();
        assert!(cli.global.yes);
    }

    #[test]
    fn yes_long_flag() {
        let cli = TestCli::try_parse_from(["test", "--yes"]).unwrap();
        assert!(cli.global.yes);
    }

    #[test]
    fn color_never() {
        let cli = TestCli::try_parse_from(["test", "--color", "never"]).unwrap();
        assert!(matches!(cli.global.color, ColorChoice::Never));
    }

    #[test]
    fn output_json() {
        let cli = TestCli::try_parse_from(["test", "--output", "json"]).unwrap();
        assert!(cli.global.is_json());
    }
}
