use clap::Parser;
use pdf_mash::config::{AppConfig, BackendChoice, PipelineMode};
use pdf_mash::eval::datasets::omnidocbench::{LanguageFilter, SampleFilter};
use pdf_mash::eval::datasets::{dataset_available, omnidocbench_dir};
use pdf_mash::eval::harness::run_eval;
use pdf_mash::eval::report::{print_summary, save_results};
use pdf_mash::pipeline::processor::Processor;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "eval_runner", about = "Run pdf-masher VLM evaluation benchmarks")]
struct Args {
    /// Model name for Ollama (e.g., glm-ocr:latest)
    #[arg(long, default_value = "glm-ocr:latest")]
    model: String,

    /// Ollama server URL
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Dataset to evaluate: omnidocbench
    #[arg(short, long, default_value = "omnidocbench")]
    dataset: String,

    /// Maximum number of samples to evaluate
    #[arg(short, long)]
    limit: Option<usize>,

    /// Language filter: english, all
    #[arg(long, default_value = "english")]
    language: String,

    /// Backend to use: ollama, cloud, or openai (aliases: vllm, sglang, mlx)
    #[arg(long)]
    backend: Option<String>,

    /// OpenAI-compatible server URL (for vLLM, SGLang, mlx-vlm, etc.)
    #[arg(long)]
    openai_url: Option<String>,

    /// DPI for PDF rendering
    #[arg(long, default_value = "200")]
    dpi: u16,

    /// Output directory for JSON results
    #[arg(short, long, default_value = "output")]
    output: String,

    /// Filter by data source (e.g., academic_literature, research_report, PPT2PDF)
    #[arg(long)]
    data_source: Option<String>,

    /// Only include samples that have tables
    #[arg(long)]
    require_tables: bool,

    /// Only include samples that have formulas
    #[arg(long)]
    require_formulas: bool,

    /// Pipeline mode: fullpage or perregion (default: perregion)
    #[arg(long)]
    mode: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Ensure we're running from the repo root
    if !std::path::Path::new("data/benchmarks").exists()
        && std::path::Path::new("../data/benchmarks").exists()
    {
        std::env::set_current_dir("..").ok();
    }

    let mut config = AppConfig {
        model: args.model.clone(),
        ollama_url: args.ollama_url.clone(),
        dpi: args.dpi,
        ..AppConfig::default()
    };

    if let Some(b) = &args.backend {
        config.backend = match b.as_str() {
            "cloud" => BackendChoice::Cloud,
            "openai" | "vllm" | "sglang" | "mlx" => BackendChoice::OpenAi,
            _ => BackendChoice::Ollama,
        };
    }
    if let Some(url) = &args.openai_url {
        config.openai_url = url.clone();
    }
    if let Some(mode) = &args.mode {
        config.pipeline_mode = match mode.as_str() {
            "fullpage" => PipelineMode::FullPage,
            _ => PipelineMode::PerRegion,
        };
    }

    let processor = Processor::new(config)?;

    match args.dataset.as_str() {
        "omnidocbench" => {
            let dir = omnidocbench_dir();
            if !dataset_available(&dir) {
                eprintln!(
                    "OmniDocBench not found at {}. Run scripts/download_datasets.sh first.",
                    dir.display()
                );
                return Ok(());
            }

            let language = match args.language.as_str() {
                "english" => LanguageFilter::English,
                _ => LanguageFilter::All,
            };

            let filter = SampleFilter {
                language: Some(language),
                data_source: args.data_source.clone(),
                require_tables: args.require_tables,
                require_formulas: args.require_formulas,
            };

            let samples = pdf_mash::eval::datasets::omnidocbench::load_omnidocbench_filtered(
                args.limit, &filter,
            )?;
            let num_samples = samples.len();
            eprintln!("Loaded {} OmniDocBench samples", num_samples);

            let start = Instant::now();
            let results = run_eval(&processor, &samples, "omnidocbench").await?;
            let elapsed = start.elapsed();
            print_summary(&results);

            let secs = elapsed.as_secs_f64();
            let per_page = if num_samples > 0 {
                secs / num_samples as f64
            } else {
                0.0
            };
            eprintln!("\n--- Timing ---");
            eprintln!("Total wall time: {:.1}s", secs);
            eprintln!("Average per page: {:.2}s", per_page);

            std::fs::create_dir_all(&args.output)?;
            let output_path = format!("{}/eval_omnidocbench_results.json", args.output);
            save_results(&results, &output_path)?;
            eprintln!("Results saved to {}", output_path);
        }
        other => {
            eprintln!("Unknown dataset: {}. Currently only 'omnidocbench' is supported.", other);
        }
    }

    Ok(())
}
