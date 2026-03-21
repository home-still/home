mod cli;

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use cli::{Cli, TopCmd};
use hs_style::mode::{self, OutputMode};
use hs_style::reporter::{Reporter, SilentReporter};
use hs_style::styles::Styles;
use hs_style::tty_reporter::TtyReporter;

fn main() -> ExitCode {
    let cli = Cli::parse();

    let mode = mode::detect(cli.global.color_str(), cli.global.is_json());

    match mode {
        OutputMode::Rich => owo_colors::set_override(true),
        _ => owo_colors::set_override(false),
    }

    let reporter: Arc<dyn Reporter> = if cli.global.quiet {
        Arc::new(SilentReporter)
    } else {
        match mode {
            OutputMode::Rich => Arc::new(TtyReporter::new(true)),
            OutputMode::Plain => Arc::new(TtyReporter::new(false)),
            OutputMode::Pipe => Arc::new(hs_style::pipe_reporter::PipeReporter),
        }
    };

    let styles = match mode {
        OutputMode::Rich => Styles::colored(),
        _ => Styles::plain(),
    };

    // Capture exit code mapper before cli.command is moved
    let exit_code_mapper: fn(&anyhow::Error) -> ExitCode = match &cli.command {
        TopCmd::Paper { .. } => paper::exit_codes::from_error,
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio  runtime");

    let result = rt.block_on(async {
        match cli.command {
            TopCmd::Paper { command } => {
                paper::commands::dispatch(command, &cli.global, &reporter, &styles).await
            }
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            reporter.error(&format!("Error: {e:#}"));
            exit_code_mapper(&e)
        }
    }
}
