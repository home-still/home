use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "pdf-mash")]
#[command(version, about = "PDF to Markdown via VLM")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Convert a PDF to markdown
    Convert {
        /// Input PDF file path
        input: PathBuf,

        /// Output markdown file path (defaults to input.md)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Use cloud API instead of local Ollama
        #[arg(long)]
        cloud: bool,

        /// Backend to use: ollama, cloud, or openai (aliases: vllm, sglang, mlx)
        #[arg(long)]
        backend: Option<String>,

        /// OpenAI-compatible server URL (for vLLM, SGLang, mlx-vlm, etc.)
        #[arg(long)]
        openai_url: Option<String>,

        /// Number of parallel requests (default: 1)
        #[arg(long)]
        parallel: Option<usize>,

        /// Pipeline mode: fullpage or perregion (default: perregion)
        #[arg(long)]
        mode: Option<String>,

        /// Max image dimension for VLM (downscale large pages, default: 1800)
        #[arg(long)]
        max_image_dim: Option<u32>,

        /// Max concurrent VLM requests across pages (default: 4)
        #[arg(long)]
        vlm_concurrency: Option<usize>,
    },

    /// Watch a directory for new PDFs and auto-convert
    Watch {
        /// Directory to watch (defaults to current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}
