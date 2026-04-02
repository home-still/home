use anyhow::{Context, Result};
use clap::Subcommand;
use hs_style::reporter::Reporter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const DEFAULT_SERVER: &str = "http://localhost:7432";
const LAYOUT_MODEL_URL: &str =
    "https://github.com/home-still/home/releases/download/v0.0.1-rc.39/pp-doclayoutv3.onnx";

fn compose_yaml(has_gpu: bool) -> String {
    let gpu_section = if has_gpu {
        "    devices:\n      - nvidia.com/gpu=all\n"
    } else {
        ""
    };
    format!(
        r#"services:
  scribe:
    image: ghcr.io/home-still/hs-scribe-server:latest
    ports:
      - "7432:7432"
    volumes:
      - ${{MODELS_DIR}}:/models:ro
    environment:
      HS_SCRIBE_LAYOUT_MODEL_PATH: /models/pp-doclayoutv3.onnx
      HS_SCRIBE_BACKEND: Ollama
      HS_SCRIBE_OLLAMA_URL: http://vlm:11434
      HS_SCRIBE_USE_CUDA: "{has_gpu}"
    depends_on:
      vlm:
        condition: service_healthy
    restart: on-failure:3

  vlm:
    image: docker.io/ollama/ollama
{gpu_section}    volumes:
      - ${{OLLAMA_DATA}}:/root/.ollama
    healthcheck:
      test: ["CMD", "ollama", "list"]
      interval: 10s
      timeout: 5s
      retries: 30
      start_period: 30s
    restart: on-failure:3
"#
    )
}

/// Compose config for macOS Apple Silicon: native Ollama (Metal GPU) on host,
/// only the scribe server runs in a container.
fn compose_yaml_native_ollama() -> String {
    r#"services:
  scribe:
    image: ghcr.io/home-still/hs-scribe-server:latest
    ports:
      - "7432:7432"
    volumes:
      - ${MODELS_DIR}:/models:ro
    environment:
      HS_SCRIBE_LAYOUT_MODEL_PATH: /models/pp-doclayoutv3.onnx
      HS_SCRIBE_BACKEND: Ollama
      HS_SCRIBE_OLLAMA_URL: http://host.docker.internal:11434
      HS_SCRIBE_USE_CUDA: "false"
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: on-failure:3
"#
    .to_string()
}

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
    /// Watch a directory for new PDFs and auto-convert to markdown
    Watch {
        /// Directory to watch for PDFs (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Output directory for markdown files (default: <dir>/markdown)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
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
        ScribeCmd::Watch {
            dir,
            output,
            server,
        } => cmd_watch(dir, output, server, reporter).await,
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

    // Quick health check so we fail fast if server isn't running
    let check_stage = reporter.begin_stage("Connecting", None);
    check_stage.set_message(&format!("server at {url}"));
    match client.health().await {
        Ok(_) => check_stage.finish_and_clear(),
        Err(e) => {
            check_stage.finish_failed("server not reachable");
            anyhow::bail!(
                "Cannot reach scribe server at {url}: {e:#}\n\nRun `hs scribe init` to set up the server."
            );
        }
    }

    let pdf_bytes =
        std::fs::read(&input).with_context(|| format!("Cannot read {}", input.display()))?;

    let stage: Arc<Box<dyn hs_style::reporter::StageHandle>> =
        Arc::new(reporter.begin_counted_stage("Converting", None));
    stage.set_message("sending PDF to server...");
    let stage_cb = Arc::clone(&stage);

    let md = client
        .convert_with_progress(pdf_bytes, move |event| {
            if event.total_pages > 0 {
                stage_cb.set_length(event.total_pages);
                stage_cb.set_position(event.page);
            }
            stage_cb.set_message(&format!("[{}] {}", event.stage, event.message));
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

// ── Watch ───────────────────────────────────────────────────────

async fn cmd_watch(
    dir: Option<PathBuf>,
    output: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use notify_debouncer_full::{new_debouncer, notify::RecursiveMode};
    use std::time::Duration;

    let url = server.as_deref().unwrap_or(DEFAULT_SERVER);
    let client = hs_scribe::client::ScribeClient::new(url);

    // Health check
    client
        .health()
        .await
        .context("Scribe server not reachable. Run `hs scribe server start` first.")?;

    let watch_dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let output_dir = output.unwrap_or_else(|| watch_dir.join("markdown"));
    std::fs::create_dir_all(&output_dir)?;

    reporter.status(
        "Watching",
        &format!("{} → {}", watch_dir.display(), output_dir.display()),
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(500), None, tx)?;
    debouncer.watch(&watch_dir, RecursiveMode::Recursive)?;

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                for event in events {
                    for path in &event.paths {
                        if path.extension().is_some_and(|ext| ext == "pdf") {
                            // Skip if markdown already exists and is newer
                            let stem = path.file_stem().unwrap_or_default();
                            let md_path = output_dir.join(format!("{}.md", stem.to_string_lossy()));
                            if md_path.exists() {
                                let pdf_mod =
                                    std::fs::metadata(path).and_then(|m| m.modified()).ok();
                                let md_mod =
                                    std::fs::metadata(&md_path).and_then(|m| m.modified()).ok();
                                if let (Some(p), Some(m)) = (pdf_mod, md_mod) {
                                    if m >= p {
                                        continue; // markdown is up to date
                                    }
                                }
                            }
                            convert_and_save(&client, path, &output_dir, reporter).await;
                        }
                    }
                }
            }
            Ok(Err(errs)) => {
                for e in errs {
                    reporter.warn(&format!("Watch error: {e}"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

async fn convert_and_save(
    client: &hs_scribe::client::ScribeClient,
    pdf_path: &std::path::Path,
    output_dir: &std::path::Path,
    reporter: &Arc<dyn Reporter>,
) {
    let stem = pdf_path.file_stem().unwrap_or_default().to_string_lossy();
    let output_path = output_dir.join(format!("{stem}.md"));

    let stage: Arc<Box<dyn hs_style::reporter::StageHandle>> =
        Arc::new(reporter.begin_counted_stage(&stem, None));
    stage.set_message("reading PDF...");
    let stage_cb = Arc::clone(&stage);

    let pdf_bytes = match std::fs::read(pdf_path) {
        Ok(b) => b,
        Err(e) => {
            stage.finish_failed(&format!("Cannot read: {e}"));
            return;
        }
    };

    let result = client
        .convert_with_progress(pdf_bytes, move |event| {
            if event.total_pages > 0 {
                stage_cb.set_length(event.total_pages);
                stage_cb.set_position(event.page);
            }
            stage_cb.set_message(&format!("[{}] {}", event.stage, event.message));
        })
        .await;

    match result {
        Ok(md) => {
            if let Err(e) = std::fs::write(&output_path, &md) {
                stage.finish_failed(&format!("Write failed: {e}"));
            } else {
                stage.finish_with_message(&format!("→ {}", output_path.display()));
            }
        }
        Err(e) => stage.finish_failed(&format!("{e:#}")),
    }
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

    eprintln!(
        "       OK ({} {})",
        compose.bin,
        compose.args_prefix.join(" ")
    );

    // Step 2: Detect GPU / Apple Silicon
    let use_native_ollama = cfg!(all(target_os = "macos", target_arch = "aarch64"));
    let has_gpu;

    if use_native_ollama {
        has_gpu = false; // No CUDA on macOS — Metal is used by native Ollama
        eprintln!("[2/5] Apple Silicon detected — using native Ollama (Metal GPU)...");

        // Install Ollama if not present
        if !check_command("ollama", &["--version"]).await {
            if check_command("brew", &["--version"]).await {
                eprintln!("       Installing Ollama via Homebrew...");
                let status = tokio::process::Command::new("brew")
                    .args(["install", "ollama"])
                    .status()
                    .await?;
                if !status.success() {
                    anyhow::bail!("Failed to install Ollama via Homebrew");
                }
            } else {
                anyhow::bail!(
                    "Ollama not found. Install from https://ollama.com or via:\n  brew install ollama"
                );
            }
        }

        // Start ollama serve if not already running
        if !check_ollama_running().await {
            eprintln!("       Starting Ollama...");
            tokio::process::Command::new("ollama")
                .arg("serve")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("Failed to start ollama serve")?;
            wait_for_ollama_native(30).await?;
        }
        eprintln!("       OK (native Ollama with Metal)");
    } else {
        eprintln!("[2/5] Detecting GPU...");
        has_gpu = check_command("nvidia-smi", &[]).await;
        if has_gpu {
            eprintln!("       NVIDIA GPU detected (CUDA enabled)");
        } else {
            eprintln!("       No NVIDIA GPU (CPU mode)");
        }
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
        let compose_content = if use_native_ollama {
            compose_yaml_native_ollama()
        } else {
            compose_yaml(has_gpu)
        };
        std::fs::write(&compose_path, compose_content)?;

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
    let output = compose.run_capture(&["-f", cf, "up", "-d"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("docker-credential-") && stderr.contains("not found") {
            anyhow::bail!(
                "Docker credential helper not found.\n\n\
                 Fix: edit ~/.docker/config.json and change\n\
                 \x20 \"credsStore\": \"desktop\"\n\
                 to\n\
                 \x20 \"credsStore\": \"\"\n\n\
                 Then re-run: hs scribe init"
            );
        }
        if stderr.contains("CDI") && stderr.contains("nvidia") {
            anyhow::bail!(
                "Docker tried to attach an NVIDIA GPU that doesn't exist on this machine.\n\n\
                 Fix: re-run with --force to regenerate the compose config without GPU:\n\
                 \x20 hs scribe init --force"
            );
        }
        if stderr.contains("certificate signed by unknown authority")
            || stderr.contains("x509: certificate")
        {
            anyhow::bail!(
                "TLS certificate error — likely a corporate VPN/proxy (e.g. Netskope, Zscaler)\n\
                 doing SSL inspection.\n\n\
                 Fix for Docker Desktop (macOS):\n\
                 \x20 1. Open Docker Desktop → Settings → General\n\
                 \x20 2. Enable \"Use system certificates\"\n\
                 \x20 3. Restart Docker Desktop\n\n\
                 If that option isn't available, manually trust the proxy CA:\n\
                 \x20 1. Find the CA cert (Netskope: /Library/Application Support/Netskope/STAgent/data/nscacert.pem)\n\
                 \x20 2. mkdir -p ~/.docker/certs.d/ghcr.io && cp <ca.pem> ~/.docker/certs.d/ghcr.io/ca.crt\n\
                 \x20 3. Repeat for docker.io if needed\n\n\
                 Then re-run: hs scribe init --force"
            );
        }
        // Print captured output so the user sees what happened
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            eprint!("{stdout}");
        }
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        anyhow::bail!("compose up failed");
    }

    // Wait for Ollama and pull model
    if use_native_ollama {
        // Native Ollama should already be running from Step 2
        eprintln!("       Pulling GLM-OCR model (first run downloads ~2.5GB)...");
        let pull_status = tokio::process::Command::new("ollama")
            .args(["pull", "glm-ocr"])
            .status()
            .await?;
        if !pull_status.success() {
            anyhow::bail!("Failed to pull glm-ocr model");
        }
    } else {
        eprintln!("       Waiting for Ollama...");
        wait_for_ollama(&compose, cf, 60).await?;
        eprintln!("       Pulling GLM-OCR model (first run downloads ~2.5GB)...");
        let pull_status = compose
            .exec_run(cf, "vlm", &["ollama", "pull", "glm-ocr"])
            .await?;
        if !pull_status.success() {
            anyhow::bail!("Failed to pull glm-ocr model into Ollama");
        }
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
    let compose = ComposeCmd::detect()
        .await
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
            // On Apple Silicon, ensure native Ollama is running
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                if !check_ollama_running().await {
                    eprintln!("Starting native Ollama...");
                    let _ = tokio::process::Command::new("ollama")
                        .arg("serve")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    wait_for_ollama_native(30).await?;
                }
            }
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

    /// Run and capture stdout+stderr (for error diagnosis).
    async fn run_capture(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut full_args: Vec<&str> = self.args_prefix.iter().map(|s| s.as_str()).collect();
        full_args.extend_from_slice(args);
        let output = tokio::process::Command::new(&self.bin)
            .args(&full_args)
            .output()
            .await?;
        Ok(output)
    }

    /// Run "exec <service> <cmd...>" via compose
    async fn exec_run(
        &self,
        compose_file: &str,
        service: &str,
        cmd: &[&str],
    ) -> Result<std::process::ExitStatus> {
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

async fn check_ollama_running() -> bool {
    reqwest::Client::new()
        .get("http://localhost:11434/api/tags")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

async fn wait_for_ollama_native(timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for native Ollama to start.\n\
                 Try running manually: ollama serve"
            );
        }
        if check_ollama_running().await {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
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

async fn wait_for_ollama(
    compose: &ComposeCmd,
    compose_file: &str,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for Ollama to start");
        }
        if compose
            .run_silent(&["-f", compose_file, "exec", "vlm", "ollama", "list"])
            .await
        {
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
