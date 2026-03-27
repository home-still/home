use anyhow::{Context, Result};
use clap::Subcommand;
use hs_style::reporter::Reporter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const DEFAULT_SERVER: &str = "http://localhost:7432";
const LAYOUT_MODEL_URL: &str =
    "https://github.com/home-still/home/releases/download/v0.0.1-rc.39/pp-doclayoutv3.onnx";

const COMPOSE_YAML: &str = r#"services:
  scribe:
    image: ghcr.io/home-still/hs-scribe-server:latest
    ports:
      - "7432:7432"
    volumes:
      - ${MODELS_DIR}:/models:ro
    environment:
      HS_SCRIBE_LAYOUT_MODEL_PATH: /models/pp-doclayoutv3.onnx
      HS_SCRIBE_BACKEND: Ollama
      HS_SCRIBE_OLLAMA_URL: http://vlm:11434
      HS_SCRIBE_USE_CUDA: "${USE_CUDA}"
    depends_on:
      vlm:
        condition: service_healthy
    restart: on-failure:3

  vlm:
    image: docker.io/ollama/ollama
    devices:
      - nvidia.com/gpu=all
    volumes:
      - ${OLLAMA_DATA}:/root/.ollama
    healthcheck:
      test: ["CMD", "ollama", "list"]
      interval: 10s
      timeout: 5s
      retries: 30
      start_period: 30s
    restart: on-failure:3
"#;

#[derive(Subcommand, Debug)]
pub enum ScribeCmd {
    /// Convert a PDF to markdown (sends to scribe server)
    Convert {
        /// Input PDF file
        input: PathBuf,
        /// Write markdown to file (default: stdout)
        #[arg(short = 'o', long = "out")]
        out_file: Option<PathBuf>,
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
    },
    /// Set up everything: download models, start Docker services
    Init {
        /// Re-download model and recreate compose config
        #[arg(long)]
        force: bool,
        /// Dry run: report what's present/missing without changing anything
        #[arg(long)]
        check: bool,
    },
    /// Manage the scribe server (Docker services)
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServerAction {
    /// Show running services and health status
    List,
    /// Start Docker services
    Start,
    /// Stop Docker services
    Stop,
    /// Health-check one or all servers
    Ping {
        /// Server URL (default: localhost:7432)
        url: Option<String>,
    },
}

pub async fn dispatch(cmd: ScribeCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        ScribeCmd::Convert {
            input,
            out_file,
            server,
        } => cmd_convert(input, out_file, server, reporter).await,
        ScribeCmd::Init { force, check } => cmd_init(force, check).await,
        ScribeCmd::Server { action } => cmd_server(action).await,
    }
}

// ── Convert ─────────────────────────────────────────────────────

async fn cmd_convert(
    input: PathBuf,
    out_file: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let url = server.as_deref().unwrap_or(DEFAULT_SERVER);
    let client = hs_scribe::client::ScribeClient::new(url);
    let pdf_bytes = std::fs::read(&input)
        .with_context(|| format!("Cannot read {}", input.display()))?;

    let stage: Arc<Box<dyn hs_style::reporter::StageHandle>> =
        Arc::new(reporter.begin_counted_stage("Converting", None));
    let stage_cb = Arc::clone(&stage);

    let md = client
        .convert_with_progress(pdf_bytes, move |event| {
            stage_cb.set_length(event.total_pages);
            stage_cb.set_position(event.page);
            stage_cb.set_message(&event.message);
        })
        .await;

    match &md {
        Ok(_) => stage.finish_with_message("done"),
        Err(e) => stage.finish_failed(&format!("{e:#}")),
    }

    let md = md?;
    match out_file {
        Some(path) => std::fs::write(&path, &md)?,
        None => print!("{md}"),
    }
    Ok(())
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(force: bool, check: bool) -> Result<()> {
    // Step 1: Check container runtime (auto-install on macOS)
    eprintln!("[1/5] Checking container runtime...");
    let mut compose = ComposeCmd::detect().await;

    if compose.is_none() {
        if cfg!(target_os = "macos") && check_command("brew", &["--version"]).await {
            eprintln!("       Not found — installing via Homebrew...");
            let steps: &[(&str, &[&str])] = &[
                ("brew", &["install", "podman", "docker-compose"]),
                ("podman", &["machine", "init", "--now"]),
            ];
            for (cmd, args) in steps {
                eprintln!("       Running: {} {}", cmd, args.join(" "));
                let status = tokio::process::Command::new(cmd)
                    .args(*args)
                    .status()
                    .await?;
                if !status.success() {
                    anyhow::bail!("{} {} failed", cmd, args[0]);
                }
            }
            compose = ComposeCmd::detect().await;
        }
    }

    if compose.is_none() {
        let instructions = if cfg!(target_os = "macos") {
            "Container runtime not found. Install Homebrew first:\n  \
             https://brew.sh\n\n\
             Then re-run: hs scribe init\n"
        } else if cfg!(target_os = "linux") {
            "Docker/Podman not found. Install with:\n\n  \
             Arch:   sudo pacman -S podman podman-compose podman-docker\n  \
             Ubuntu: https://docs.docker.com/get-docker/\n  \
             Fedora: sudo dnf install podman podman-compose podman-docker\n"
        } else {
            "Docker not found.\n  Install: https://docs.docker.com/get-docker/\n"
        };
        anyhow::bail!("{}", instructions);
    }
    let compose = compose.unwrap();

    // On macOS, ensure podman machine is running
    if cfg!(target_os = "macos") && check_command("podman", &["--version"]).await {
        let machine_running = check_command("podman", &["machine", "info"]).await;
        if !machine_running {
            eprintln!("       Starting podman machine...");
            let _ = tokio::process::Command::new("podman")
                .args(["machine", "init", "--now"])
                .status()
                .await;
        }
    }

    eprintln!("       OK ({} {})", compose.bin, compose.args_prefix.join(" "));

    // Step 2: Detect GPU
    eprintln!("[2/5] Detecting GPU...");
    let has_gpu = check_command("nvidia-smi", &[]).await;
    if has_gpu {
        eprintln!("       NVIDIA GPU detected (CUDA enabled)");
    } else {
        eprintln!("       No NVIDIA GPU (CPU mode)");
    }

    // Step 3: Download layout model
    let models_dir = data_dir().join("models");
    let layout_path = models_dir.join("pp-doclayoutv3.onnx");

    eprintln!("[3/5] Layout model...");
    if layout_path.exists() && !force {
        eprintln!("       OK (already downloaded)");
    } else if check {
        eprintln!("       MISSING ({})", layout_path.display());
    } else {
        std::fs::create_dir_all(&models_dir)?;
        eprintln!("       Downloading (~125MB)...");
        download_file(LAYOUT_MODEL_URL, &layout_path).await?;
        eprintln!("       Saved to {}", layout_path.display());
    }

    // Step 4: Write compose config
    let config_dir = config_dir();
    let compose_path = config_dir.join("docker-compose.yml");
    let env_path = config_dir.join(".env");

    eprintln!("[4/5] Docker Compose config...");
    if compose_path.exists() && !force {
        eprintln!("       OK (already exists)");
    } else if check {
        if compose_path.exists() {
            eprintln!("       OK ({})", compose_path.display());
        } else {
            eprintln!("       MISSING");
        }
    } else {
        std::fs::create_dir_all(&config_dir)?;
        std::fs::write(&compose_path, COMPOSE_YAML)?;

        let env_contents = format!(
            "MODELS_DIR={}\nOLLAMA_DATA={}\nUSE_CUDA={}\n",
            models_dir.display(),
            data_dir().join("ollama").display(),
            has_gpu,
        );
        std::fs::write(&env_path, env_contents)?;
        eprintln!("       Written to {}", compose_path.display());
    }

    if check {
        // Step 5 (check only): report service status
        eprintln!("[5/5] Service status...");
        match health_check(DEFAULT_SERVER).await {
            Ok(h) => eprintln!(
                "       Scribe server: OK (layout={}, tables={})",
                h.layout_model, h.table_model
            ),
            Err(_) => eprintln!("       Scribe server: NOT RUNNING"),
        }
        return Ok(());
    }

    // Step 5: Start services
    eprintln!("[5/5] Starting services...");
    let cf = compose_path.to_str().unwrap_or_default();
    let status = compose.run(&["-f", cf, "up", "-d"]).await?;
    if !status.success() {
        anyhow::bail!("compose up failed");
    }

    // Wait for Ollama to be ready, then pull the model
    eprintln!("       Waiting for Ollama...");
    wait_for_ollama(&compose, cf, 60).await?;
    eprintln!("       Pulling GLM-OCR model (first run downloads ~2.5GB)...");
    let pull_status = compose.exec_run(cf, "vlm", &["ollama", "pull", "glm-ocr"]).await?;
    if !pull_status.success() {
        anyhow::bail!("Failed to pull glm-ocr model into Ollama");
    }
    eprintln!("       Waiting for scribe server...");
    wait_for_health(DEFAULT_SERVER, 120).await?;
    eprintln!("       Scribe server: OK");

    eprintln!();
    eprintln!("Ready! Try: hs scribe convert paper.pdf");
    eprintln!();
    eprintln!("To stop:  hs scribe server stop");
    eprintln!("To check: hs scribe server list");
    Ok(())
}

// ── Server ──────────────────────────────────────────────────────

async fn cmd_server(action: ServerAction) -> Result<()> {
    let compose_path = config_dir().join("docker-compose.yml");
    if !compose_path.exists() {
        anyhow::bail!("No compose config found. Run `hs scribe init` first.");
    }
    let compose = ComposeCmd::detect().await
        .ok_or_else(|| anyhow::anyhow!("No container runtime found"))?;
    let cf = compose_path.to_str().unwrap_or_default();

    match action {
        ServerAction::List => {
            let _ = compose.run(&["-f", cf, "ps"]).await?;
            eprintln!();
            match health_check(DEFAULT_SERVER).await {
                Ok(h) => eprintln!(
                    "Health: OK (layout={}, tables={})",
                    h.layout_model, h.table_model
                ),
                Err(_) => eprintln!("Health: NOT REACHABLE"),
            }
        }
        ServerAction::Start => {
            compose.run(&["-f", cf, "up", "-d"]).await?;
            eprintln!("Waiting for services...");
            wait_for_health(DEFAULT_SERVER, 300).await?;
            eprintln!("Ready.");
        }
        ServerAction::Stop => {
            compose.run(&["-f", cf, "down"]).await?;
            eprintln!("Stopped.");
        }
        ServerAction::Ping { url } => {
            let target = url.as_deref().unwrap_or(DEFAULT_SERVER);
            match health_check(target).await {
                Ok(h) => eprintln!(
                    "{}: OK (layout={}, tables={})",
                    target, h.layout_model, h.table_model
                ),
                Err(e) => eprintln!("{}: FAILED ({})", target, e),
            }
        }
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────

fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("home-still")
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("home-still")
}

/// Detected compose command: "docker compose", "docker-compose", or "podman-compose"
struct ComposeCmd {
    bin: String,
    args_prefix: Vec<String>,
}

impl ComposeCmd {
    async fn detect() -> Option<Self> {
        // docker compose (v2 plugin)
        if check_command("docker", &["compose", "version"]).await {
            return Some(Self {
                bin: "docker".into(),
                args_prefix: vec!["compose".into()],
            });
        }
        // podman compose (delegates to external provider)
        if check_command("podman", &["compose", "version"]).await {
            return Some(Self {
                bin: "podman".into(),
                args_prefix: vec!["compose".into()],
            });
        }
        // docker-compose standalone
        if check_command("docker-compose", &["version"]).await {
            return Some(Self {
                bin: "docker-compose".into(),
                args_prefix: vec![],
            });
        }
        // podman-compose standalone
        if check_command("podman-compose", &["version"]).await {
            return Some(Self {
                bin: "podman-compose".into(),
                args_prefix: vec![],
            });
        }
        None
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::ExitStatus> {
        let mut full_args: Vec<&str> = self.args_prefix.iter().map(|s| s.as_str()).collect();
        full_args.extend_from_slice(args);
        let status = tokio::process::Command::new(&self.bin)
            .args(&full_args)
            .status()
            .await?;
        Ok(status)
    }

    async fn run_silent(&self, args: &[&str]) -> bool {
        let mut full_args: Vec<&str> = self.args_prefix.iter().map(|s| s.as_str()).collect();
        full_args.extend_from_slice(args);
        tokio::process::Command::new(&self.bin)
            .args(&full_args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Run "exec <service> <cmd...>" via compose
    async fn exec_run(&self, compose_file: &str, service: &str, cmd: &[&str]) -> Result<std::process::ExitStatus> {
        let mut args = vec!["-f", compose_file, "exec", service];
        args.extend_from_slice(cmd);
        self.run(&args).await
    }
}

async fn check_command(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn download_file(url: &str, dest: &std::path::Path) -> Result<()> {
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed ({}): {}", resp.status(), url);
    }
    let bytes = resp.bytes().await?;
    let mut file = tokio::fs::File::create(dest).await?;
    file.write_all(&bytes).await?;
    Ok(())
}

async fn health_check(server_url: &str) -> Result<hs_scribe::client::HealthResponse> {
    let client = hs_scribe::client::ScribeClient::new(server_url);
    client.health().await
}

async fn wait_for_ollama(compose: &ComposeCmd, compose_file: &str, timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for Ollama to start");
        }
        if compose.run_silent(&["-f", compose_file, "exec", "vlm", "ollama", "list"]).await {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

async fn wait_for_health(server_url: &str, timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for server at {} ({}s). \
                 Check `docker compose logs` for errors.",
                server_url,
                timeout_secs
            );
        }
        if health_check(server_url).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}
