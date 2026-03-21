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
    #[arg(long, global = true, default_value = "auto")]
    pub color: ColorChoice,

    /// Output format: text|json|ndjson
    #[arg(long, global = true, default_value = "text")]
    pub output: OutputFormat,

    /// Suppress all non-result output
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Show debug-level output
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Override config directory
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,
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
