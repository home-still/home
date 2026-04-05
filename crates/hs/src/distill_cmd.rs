use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::compose::{check_command, wait_for_url, ComposeCmd};
use hs_common::reporter::Reporter;
use hs_distill::cli::{DistillCmd, DistillServerAction};
use hs_distill::client::DistillClient;
use hs_distill::config::{DistillClientConfig, DistillServerConfig};

const DEFAULT_SERVER: &str = "http://localhost:7434";
const QDRANT_REST_PORT: u16 = 6333;
const QDRANT_GRPC_PORT: u16 = 6334;

fn resolve_servers(cli_server: Option<&str>) -> Vec<String> {
    if let Some(s) = cli_server {
        return vec![s.to_string()];
    }
    match DistillClientConfig::load() {
        Ok(cfg) => {
            if cfg.servers.is_empty() {
                vec![DEFAULT_SERVER.to_string()]
            } else {
                cfg.servers
            }
        }
        Err(e) => {
            eprintln!("warning: Failed to load distill config: {e}, using default server");
            vec![DEFAULT_SERVER.to_string()]
        }
    }
}

fn hidden_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
}

fn distill_compose_path() -> PathBuf {
    hidden_dir().join("docker-compose-distill.yml")
}

fn distill_pid_path() -> PathBuf {
    hidden_dir().join("distill-server.pid")
}

/// Convert Qdrant gRPC URL (:6334) to REST URL (:6333) for health checks.
fn qdrant_rest_from_grpc(grpc_url: &str) -> String {
    grpc_url.replace(
        &format!(":{QDRANT_GRPC_PORT}"),
        &format!(":{QDRANT_REST_PORT}"),
    )
}

fn distill_compose_yaml(data_dir: &std::path::Path) -> String {
    format!(
        r#"services:
  qdrant:
    image: docker.io/qdrant/qdrant:latest
    ports:
      - "{QDRANT_REST_PORT}:{QDRANT_REST_PORT}"
      - "{QDRANT_GRPC_PORT}:{QDRANT_GRPC_PORT}"
    volumes:
      - {}:/qdrant/storage
    restart: on-failure:3
"#,
        data_dir.display()
    )
}

fn find_distill_binary() -> Option<PathBuf> {
    // Check ~/.local/bin (install script location)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-distill-server");
        if path.exists() {
            return Some(path);
        }
    }
    // Check next to the current binary (same install dir)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-distill-server");
            if path.exists() {
                return Some(path);
            }
        }
    }
    // Check cargo target dirs (dev builds)
    let project = hs_common::resolve_project_dir();
    for profile in ["release", "debug"] {
        let path = project
            .join("target")
            .join(profile)
            .join("hs-distill-server");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

pub async fn dispatch(cmd: DistillCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        DistillCmd::Init { force } => cmd_init(force, reporter).await,
        DistillCmd::Server { action } => cmd_server(action, reporter).await,
        DistillCmd::Index {
            force: _,
            file,
            server,
        } => cmd_index(file, server.as_deref(), reporter).await,
        DistillCmd::Search {
            query,
            limit,
            year,
            topic,
            server,
        } => cmd_search(&query, limit, year, topic, server.as_deref()).await,
        DistillCmd::Status { server } => cmd_status(server.as_deref(), reporter).await,
    }
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(force: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load().unwrap_or_default();
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Step 1: Check Qdrant availability
    reporter.status("Step 1/3", "Checking Qdrant availability");
    let qdrant_reachable = reqwest::get(&format!("{qdrant_rest}/healthz"))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    if qdrant_reachable && !force {
        reporter.status("Qdrant", &format!("already reachable at {qdrant_rest}"));
    } else {
        // Need Docker for Qdrant
        let compose = ComposeCmd::detect().await;
        if compose.is_none() {
            if cfg!(target_os = "macos") && check_command("brew", &["--version"]).await {
                anyhow::bail!(
                    "No container runtime found. Install with:\n  \
                     brew install podman docker-compose\n  \
                     podman machine init && podman machine start"
                );
            }
            anyhow::bail!(
                "No container runtime found. Install Docker or Podman:\n  \
                 https://docs.docker.com/get-docker/"
            );
        }
        let compose = compose.unwrap();
        reporter.status(
            "Runtime",
            &format!("{} {}", compose.bin, compose.args_prefix.join(" ")),
        );

        // Step 2: Write compose config
        reporter.status("Step 2/3", "Docker Compose config");
        let compose_path = distill_compose_path();

        if compose_path.exists() && !force {
            reporter.status("Config", "already exists");
        } else {
            std::fs::create_dir_all(hidden_dir())?;
            std::fs::create_dir_all(&config.qdrant_data_dir)?;
            std::fs::write(&compose_path, distill_compose_yaml(&config.qdrant_data_dir))?;

            reporter.status("Written", &format!("{}", compose_path.display()));
        }

        // Step 3: Start Qdrant
        reporter.status("Step 3/3", "Starting Qdrant");
        let cf = compose_path.to_string_lossy().to_string();
        compose.run(&["-f", &cf, "up", "-d"]).await?;
        wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    }

    // Check for distill binary
    if find_distill_binary().is_none() {
        reporter.warn(
            "hs-distill-server binary not found. Build with:\n  \
             cargo build --release -p hs-distill --features server",
        );
    } else {
        reporter.status("Binary", "hs-distill-server found");
    }

    reporter.finish("Ready! Run: hs distill server start");
    Ok(())
}

// ── Server ──────────────────────────────────────────────────────

async fn cmd_server(action: DistillServerAction, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match action {
        DistillServerAction::Start => cmd_server_start(reporter).await,
        DistillServerAction::Stop => cmd_server_stop(reporter).await,
        DistillServerAction::Ping { url } => {
            let target = url.as_deref().unwrap_or(DEFAULT_SERVER);
            let client = DistillClient::new(target);
            match client.health().await {
                Ok(h) => reporter.status("OK", &format!("{target} ({})", h.compute_device)),
                Err(e) => reporter.error(&format!("{target}: {e}")),
            }
            Ok(())
        }
    }
}

async fn cmd_server_start(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load().unwrap_or_default();
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // 1. Start Qdrant container if compose file exists
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        let compose = ComposeCmd::detect()
            .await
            .ok_or_else(|| anyhow::anyhow!("No container runtime found. Run: hs distill init"))?;
        let cf = compose_path.to_string_lossy().to_string();
        compose.run(&["-f", &cf, "up", "-d"]).await?;
        wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    } else {
        // No compose file — check if Qdrant is reachable anyway
        let reachable = reqwest::get(&format!("{qdrant_rest}/healthz"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !reachable {
            anyhow::bail!(
                "Qdrant not reachable at {qdrant_rest} and no compose config found.\n\
                 Run: hs distill init"
            );
        }
        reporter.status("Qdrant", &format!("reachable at {qdrant_rest}"));
    }

    // 2. Start native distill server
    let pid_path = distill_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            reporter.status("Distill", &format!("already running (PID {pid})"));
            return Ok(());
        }
    }

    let binary = find_distill_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "hs-distill-server binary not found. Build with:\n  \
             cargo build --release -p hs-distill --features server"
        )
    })?;

    let log_dir = hs_common::resolve_log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("distill-server.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let child = std::process::Command::new(&binary)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("Failed to start distill server")?;

    let pid = child.id();
    std::fs::write(&pid_path, pid.to_string())?;

    let distill_url = format!("http://{}:{}/health", config.host, config.port);
    // Server binds to 0.0.0.0 but we check on localhost
    let check_url = format!("http://localhost:{}/health", config.port);
    wait_for_url(&check_url, 60, "distill server")
        .await
        .context(format!(
            "Distill server started (PID {pid}) but health check failed.\n\
         Check logs: {}",
            log_path.display()
        ))?;

    reporter.status("Distill", &format!("OK (PID {pid})"));
    reporter.finish(&format!(
        "Listening on {distill_url}\nLogs: {}",
        log_path.display()
    ));
    Ok(())
}

async fn cmd_server_stop(reporter: &Arc<dyn Reporter>) -> Result<()> {
    // 1. Stop native distill server
    let pid_path = distill_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            #[cfg(unix)]
            {
                // SIGTERM first, then wait, then SIGKILL
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                for _ in 0..50 {
                    if !crate::daemon::is_process_alive(pid) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if crate::daemon::is_process_alive(pid) {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
            crate::daemon::remove_pid_file(&pid_path);
            reporter.status("Distill", &format!("stopped (PID {pid})"));
        } else {
            crate::daemon::remove_pid_file(&pid_path);
            reporter.status("Distill", "not running (stale PID removed)");
        }
    } else {
        reporter.status("Distill", "not running");
    }

    // 2. Stop Qdrant container
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        if let Some(compose) = ComposeCmd::detect().await {
            let cf = compose_path.to_string_lossy().to_string();
            compose.run(&["-f", &cf, "down"]).await?;
            reporter.status("Qdrant", "stopped");
        }
    }

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────

async fn cmd_status(server: Option<&str>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load().unwrap_or_default();
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Qdrant health
    let qdrant_ok = reqwest::get(&format!("{qdrant_rest}/healthz"))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if qdrant_ok {
        reporter.status("Qdrant", &format!("OK ({qdrant_rest})"));
    } else {
        reporter.error(&format!("Qdrant: not reachable at {qdrant_rest}"));
    }

    // Distill server PID
    let pid_path = distill_pid_path();
    match crate::daemon::read_pid(&pid_path) {
        Some(pid) if crate::daemon::is_process_alive(pid) => {
            reporter.status("Server", &format!("running (PID {pid})"));
        }
        _ => {
            reporter.status("Server", "not running");
        }
    }

    // Collection info (if server is reachable)
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);
    match client.status().await {
        Ok(status) => {
            reporter.status("Collection", &status.collection);
            reporter.status("Points", &status.points_count.to_string());
            reporter.status("Device", &status.compute_device);
        }
        Err(_) => {
            reporter.status(
                "Collection",
                &format!("unavailable (server at {} not reachable)", servers[0]),
            );
        }
    }

    // Compose status if available
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        if let Some(compose) = ComposeCmd::detect().await {
            let cf = compose_path.to_string_lossy().to_string();
            let _ = compose.run(&["-f", &cf, "ps"]).await;
        }
    }

    Ok(())
}

// ── Index ───────────────────────────────────────────────────────

async fn cmd_index(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);

    // Health check
    match client.health().await {
        Ok(h) => {
            reporter.status(
                "Connected",
                &format!("{} ({})", servers[0], h.compute_device),
            );
        }
        Err(e) => {
            return Err(e).context(format!("Is hs-distill-server running at {}?", servers[0]));
        }
    };

    // Determine files to index
    let config = DistillClientConfig::load().unwrap_or_default();
    let markdown_dir = config.markdown_dir;

    let paths: Vec<PathBuf> = if let Some(files) = files {
        files
    } else {
        std::fs::read_dir(&markdown_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                    .collect()
            })
            .unwrap_or_default()
    };

    if paths.is_empty() {
        reporter.warn("No markdown files found to index");
        return Ok(());
    }

    reporter.status(
        "Found",
        &format!("{} files to index (press q to stop early)", paths.len()),
    );

    let mut total_chunks = 0u32;
    let mut indexed_count = 0usize;
    let mut stopped_early = false;

    for path in &paths {
        // Briefly enable raw mode to check for 'q' keypress, then disable
        // so indicatif progress bars render correctly
        if crossterm::terminal::enable_raw_mode().is_ok() {
            while crossterm::event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                    if key.kind == crossterm::event::KeyEventKind::Press
                        && matches!(
                            key.code,
                            crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                        )
                    {
                        stopped_early = true;
                    }
                }
            }
            let _ = crossterm::terminal::disable_raw_mode();
        }
        if stopped_early {
            break;
        }

        let path_str = path.to_string_lossy().to_string();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        match client
            .index_file_with_progress(&path_str, |_progress| {})
            .await
        {
            Ok(result) => {
                total_chunks += result.chunks_indexed;
                indexed_count += 1;
                eprintln!(
                    "  [{}/{}] {} — {} chunks",
                    indexed_count,
                    paths.len(),
                    stem,
                    result.chunks_indexed
                );
            }
            Err(e) => {
                eprintln!(
                    "  [{}/{}] {} — error: {e}",
                    indexed_count + 1,
                    paths.len(),
                    stem
                );
            }
        }
    }

    let summary = if stopped_early {
        format!(
            "Stopped: indexed {}/{} files, {} chunks",
            indexed_count,
            paths.len(),
            total_chunks
        )
    } else {
        format!(
            "Indexed {} files, {} total chunks",
            paths.len(),
            total_chunks
        )
    };
    reporter.finish(&summary);

    Ok(())
}

// ── Search ──────────────────────────────────────────────────────

async fn cmd_search(
    query: &str,
    limit: u64,
    year: Option<String>,
    topic: Option<String>,
    server: Option<&str>,
) -> Result<()> {
    let servers = resolve_servers(server);
    let client = DistillClient::new(&servers[0]);

    let filters = hs_distill::client::SearchFilters { year, topic };
    let hits = client
        .search(query, limit, filters)
        .await
        .context(format!("Is hs-distill-server running at {}?", servers[0]))?;

    if hits.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    for (i, hit) in hits.iter().enumerate() {
        let title = hit.title.as_deref().unwrap_or(&hit.doc_id);
        let page_info = hit
            .page
            .map(|p| format!(" (page {})", p))
            .unwrap_or_default();
        let pdf = hit.pdf_path.as_deref().unwrap_or("?");

        println!("\n{}. {} [score: {:.3}]", i + 1, title, hit.score);
        println!(
            "   {}:{}–{}{}",
            hit.doc_id, hit.line_start, hit.line_end, page_info
        );
        println!("   PDF: {pdf}");

        let preview: String = hit.chunk_text.chars().take(200).collect();
        println!("   {preview}...");
    }

    Ok(())
}
