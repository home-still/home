use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Subcommand, Debug)]
pub enum ScribeCmd {
    /// Convert a PDF to markdown
    Convert {
        /// Input PDF file
        input: PathBuf,
        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
    },
    /// Download models and check dependencies
    Init {
        /// Re-download even if models exist
        #[arg(long)]
        force: bool,
        /// Dry run: report what's missing
        #[arg(long)]
        check: bool,
    },
}

const DEFAULT_SERVER: &str = "http://localhost:7432";

pub async fn dispatch(cmd: ScribeCmd) -> Result<()> {
    match cmd {
        ScribeCmd::Convert {
            input,
            output,
            server,
        } => {
            let url = server.as_deref().unwrap_or(DEFAULT_SERVER);
            let client = hs_scribe::client::ScribeClient::new(url);

            let pdf_bytes = std::fs::read(&input)?;
            let md = client.convert(pdf_bytes).await?;

            match output {
                Some(path) => std::fs::write(&path, &md)?,
                None => print!("{md}"),
            }
            Ok(())
        }
        ScribeCmd::Init { force, check } => {
            let models_dir = dirs::data_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
                .join("home-still")
                .join("models");

            if check {
                eprintln!("Models dir: {}", models_dir.display());
            }

            if !check {
                std::fs::create_dir_all(&models_dir)?;
            }

            let layout_path = models_dir.join("pp-doclayoutv3.onnx");
            let table_path = models_dir.join("slanet-plus.onnx");

            // Check layout model
            if layout_path.exists() && !force {
                eprintln!("Layout model: OK ({})", layout_path.display());
            } else if check {
                eprintln!("Layout model: MISSING");
            } else {
                eprintln!("Downloading layout model...");
                download_model(
                    "https://huggingface.co/opendatalab/PP-DocLayout-v3/resolve/main/pp-doclayoutv3.onnx",
                    &layout_path,
                )
                .await?;
                eprintln!("Layout model: OK");
            }

            // Check table model
            if table_path.exists() && !force {
                eprintln!("Table model: OK ({})", table_path.display());
            } else if check {
                eprintln!("Table model: MISSING");
            } else {
                eprintln!("Downloading table model...");
                download_model(
                    "https://paddleocr.bj.bcebos.com/ppstructure/models/slanet/slanet-plus.onnx",
                    &table_path,
                )
                .await?;
                eprintln!("Table model: OK");
            }

            Ok(())
        }
    }
}

async fn download_model(url: &str, dest: &std::path::Path) -> Result<()> {
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed ({}): {}", resp.status(), url);
    }

    let bytes = resp.bytes().await?;
    let mut file = tokio::fs::File::create(dest).await?;
    file.write_all(&bytes).await?;
    eprintln!("  Saved to {}", dest.display());
    Ok(())
}
