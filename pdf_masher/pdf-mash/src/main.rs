use anyhow::Result;
use clap::Parser;
use pdf_mash::cli::args::{Args, Command};
use pdf_mash::config::{AppConfig, BackendChoice, PipelineMode};
use pdf_mash::pipeline::processor::Processor;
use std::fs;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mut config = AppConfig::load().unwrap_or_else(|e| {
        tracing::warn!("Config load error: {e}, using defaults");
        AppConfig::default()
    });

    match args.command {
        Command::Convert {
            input,
            output,
            cloud,
            backend,
            openai_url,
            parallel,
            mode,
            max_image_dim,
            vlm_concurrency,
        } => {
            if cloud {
                config.backend = BackendChoice::Cloud;
            }
            if let Some(b) = backend {
                config.backend = match b.as_str() {
                    "cloud" => BackendChoice::Cloud,
                    "openai" | "vllm" | "sglang" | "mlx" => BackendChoice::OpenAi,
                    _ => BackendChoice::Ollama,
                };
            }
            if let Some(url) = openai_url {
                config.openai_url = url;
            }
            if let Some(p) = parallel {
                config.parallel = p;
            }
            if let Some(m) = mode {
                config.pipeline_mode = match m.as_str() {
                    "fullpage" | "full" => PipelineMode::FullPage,
                    _ => PipelineMode::PerRegion,
                };
            }
            if let Some(d) = max_image_dim {
                config.max_image_dim = d;
            }
            if let Some(c) = vlm_concurrency {
                config.vlm_concurrency = c;
            }

            let output_path = output.unwrap_or_else(|| input.with_extension("md"));

            println!("Processing: {}", input.display());
            let processor = Processor::new(config)?;
            let markdown = processor.process_pdf(
                input.to_str().ok_or_else(|| anyhow::anyhow!("Input path is not valid UTF-8: {}", input.display()))?
            ).await?;
            fs::write(&output_path, &markdown)?;
            println!("Done! Saved to: {}", output_path.display());
        }

        Command::Watch { dir } => {
            pdf_mash::watch::watch_directory(&dir, config).await?;
        }
    }

    Ok(())
}
