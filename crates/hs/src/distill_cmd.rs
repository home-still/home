use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::auth::client::is_cloud_url;
use hs_common::compose::{check_command, wait_for_url, ComposeCmd};
use hs_common::global_args::{GlobalArgs, OutputFormat};
use hs_common::reporter::Reporter;
use hs_distill::cli::{DistillCmd, DistillServerAction};
use hs_distill::client::DistillClient;
use hs_distill::config::{DistillClientConfig, DistillServerConfig};

const DEFAULT_SERVER: &str = "http://localhost:7434";

/// Create a DistillClient, with auth headers if the URL is a cloud gateway.
async fn make_distill_client(url: &str) -> Result<DistillClient> {
    if is_cloud_url(url) {
        let auth = hs_common::auth::client::AuthenticatedClient::from_default_path()
            .context("Cloud credentials not found. Run `hs cloud enroll` first.")?;
        let http = auth.build_reqwest_client().await?;
        Ok(DistillClient::new_with_client(url, http))
    } else {
        Ok(DistillClient::new(url))
    }
}
const QDRANT_REST_PORT: u16 = 6333;
const QDRANT_GRPC_PORT: u16 = 6334;

async fn resolve_servers(cli_server: Option<&str>) -> Vec<String> {
    if let Some(s) = cli_server {
        return vec![s.to_string()];
    }
    let config_servers = match DistillClientConfig::load() {
        Ok(cfg) if !cfg.servers.is_empty() => cfg.servers,
        _ => vec![DEFAULT_SERVER.to_string()],
    };
    hs_common::service::registry::discover_or_fallback("distill", config_servers).await
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

/// Find the ort cache directory containing CUDA provider .so files.
fn find_ort_cuda_libs() -> Option<String> {
    let cache = dirs::home_dir()?.join(".cache/ort.pyke.io/dfbin");
    if !cache.exists() {
        return None;
    }
    // Walk into the platform-specific subdir to find libonnxruntime_providers_cuda.so
    for entry in std::fs::read_dir(&cache).ok()?.flatten() {
        let platform_dir = entry.path();
        for hash_entry in std::fs::read_dir(&platform_dir).ok()?.flatten() {
            let dir = hash_entry.path();
            if dir.join("libonnxruntime_providers_cuda.so").exists() {
                return Some(dir.to_string_lossy().to_string());
            }
        }
    }
    None
}

pub async fn dispatch(
    cmd: DistillCmd,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    match cmd {
        DistillCmd::Init { force } => cmd_init(force, reporter).await,
        DistillCmd::Server { action } => {
            let hint = match &action {
                DistillServerAction::Start => "hs serve distill start",
                DistillServerAction::Stop => "hs serve distill stop",
                DistillServerAction::Ping { .. } => "hs serve distill",
            };
            eprintln!("warning: `hs distill server` is deprecated, use `{hint}` instead");
            cmd_server(action, reporter).await
        }
        DistillCmd::Index {
            force,
            file,
            server,
            no_yield,
            daemon_child,
        } => {
            if daemon_child {
                cmd_index_daemon(file, server.as_deref(), no_yield, force).await
            } else {
                cmd_index(file, server.as_deref(), no_yield, force, reporter).await
            }
        }
        DistillCmd::Search {
            query,
            limit,
            year,
            topic,
            server,
        } => cmd_search(&query, limit, year, topic, server.as_deref(), global).await,
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

pub async fn cmd_server_start(reporter: &Arc<dyn Reporter>) -> Result<()> {
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

    // 2. Stop any existing distill server (e.g. orphaned from a previous run)
    let pid_path = distill_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            reporter.status("Distill", &format!("stopping old process (PID {pid})"));
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !crate::daemon::is_process_alive(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            #[cfg(unix)]
            if crate::daemon::is_process_alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            crate::daemon::remove_pid_file(&pid_path);
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

    // Ensure ONNX Runtime CUDA provider .so files can be found at runtime.
    // They live in the ort cache dir alongside the static lib.
    let mut ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if let Some(cache_dir) = find_ort_cuda_libs() {
        if !ld_path.contains(&cache_dir) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&cache_dir);
        }
    }
    // Include standard CUDA and local lib paths
    for extra in [
        "/usr/local/lib",
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
    ] {
        if !ld_path.contains(extra) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(extra);
        }
    }
    // Include ~/.local/lib for user-created compat symlinks
    if let Some(home) = dirs::home_dir() {
        let user_lib = home.join(".local/lib");
        let user_lib_str = user_lib.to_string_lossy().to_string();
        if !ld_path.contains(&user_lib_str) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&user_lib_str);
        }
    }

    // For ort load-dynamic: tell it where to find libonnxruntime.so
    let ort_dylib = std::env::var("ORT_DYLIB_PATH")
        .unwrap_or_else(|_| "/usr/lib/libonnxruntime.so".to_string());

    // fastembed defaults cache_dir to CWD/.fastembed_cache, which breaks
    // when launched from systemd (CWD = /). Use a stable absolute path.
    let fastembed_cache = std::env::var("FASTEMBED_CACHE_DIR").unwrap_or_else(|_| {
        // Check next to the binary first (where old versions cached the model)
        let beside_binary = binary.parent().unwrap_or(binary.as_ref()).join(".fastembed_cache");
        if beside_binary.exists() {
            return beside_binary.to_string_lossy().to_string();
        }
        // Otherwise use ~/.home-still/fastembed_cache
        hidden_dir()
            .join("fastembed_cache")
            .to_string_lossy()
            .to_string()
    });

    let child = std::process::Command::new(&binary)
        .env("LD_LIBRARY_PATH", &ld_path)
        .env("ORT_DYLIB_PATH", &ort_dylib)
        .env("FASTEMBED_CACHE_DIR", &fastembed_cache)
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
    wait_for_url(&check_url, 300, "distill server")
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

pub async fn cmd_server_stop(reporter: &Arc<dyn Reporter>) -> Result<()> {
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

// ── Public API for `hs serve` ──────────────────────────────────

/// Idempotent init: ensures Qdrant and compose config are ready.
/// Skips steps that are already done. Does NOT start the distill server.
pub async fn ensure_init(reporter: &Arc<dyn Reporter>) -> Result<()> {
    cmd_init(false, reporter).await
}

/// Start the distill server in the foreground (blocks until shutdown).
/// Runs the native binary directly instead of as a background daemon.
pub async fn start_server_foreground(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load().unwrap_or_default();
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Ensure Qdrant is running
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        let compose = ComposeCmd::detect()
            .await
            .ok_or_else(|| anyhow::anyhow!("No container runtime found. Run: hs distill init"))?;
        let cf = compose_path.to_string_lossy().to_string();
        compose.run(&["-f", &cf, "up", "-d"]).await?;
        hs_common::compose::wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    } else {
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
    }

    let binary = find_distill_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "hs-distill-server binary not found. Build with:\n  \
             cargo build --release -p hs-distill --features server"
        )
    })?;

    // Build environment (same logic as cmd_server_start)
    let mut ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if let Some(cache_dir) = find_ort_cuda_libs() {
        if !ld_path.contains(&cache_dir) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&cache_dir);
        }
    }
    for extra in [
        "/usr/local/lib",
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
    ] {
        if !ld_path.contains(extra) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(extra);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let user_lib = home.join(".local/lib");
        let user_lib_str = user_lib.to_string_lossy().to_string();
        if !ld_path.contains(&user_lib_str) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&user_lib_str);
        }
    }

    let ort_dylib = std::env::var("ORT_DYLIB_PATH")
        .unwrap_or_else(|_| "/usr/lib/libonnxruntime.so".to_string());

    let fastembed_cache = std::env::var("FASTEMBED_CACHE_DIR").unwrap_or_else(|_| {
        let beside_binary = binary.parent().unwrap_or(binary.as_ref()).join(".fastembed_cache");
        if beside_binary.exists() {
            return beside_binary.to_string_lossy().to_string();
        }
        hidden_dir()
            .join("fastembed_cache")
            .to_string_lossy()
            .to_string()
    });

    reporter.status(
        "Distill",
        &format!("running on port {port} (Ctrl+C to stop)"),
    );

    // Run in foreground — inherit stdout/stderr, block until exit
    let status = tokio::process::Command::new(&binary)
        .env("LD_LIBRARY_PATH", &ld_path)
        .env("ORT_DYLIB_PATH", &ort_dylib)
        .env("FASTEMBED_CACHE_DIR", &fastembed_cache)
        .env("HS_DISTILL_PORT", port.to_string())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to start distill server")?;

    if !status.success() {
        anyhow::bail!("distill server exited with {status}");
    }

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────

/// Ensure the index daemon is running. Spawns it if not already active.
/// Used by the pipeline auto-trigger: scribe watch → distill index.
/// Skips if distill server is not reachable (avoids spawning a daemon that
/// will immediately fail its health check).
pub async fn ensure_index_running() -> bool {
    let pid_path = index_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            return true; // already running
        }
        crate::daemon::remove_pid_file(&pid_path);
    }

    // Quick check: is the distill server binary available?
    if find_distill_binary().is_none() {
        tracing::debug!("Skipping auto-index: hs-distill-server binary not found");
        return false;
    }

    // Quick check: is the distill server reachable? Uses HTTP health check
    // so it works for both local and remote servers (e.g. big_mac → big).
    let server_url = DistillClientConfig::load()
        .ok()
        .and_then(|cfg| cfg.servers.into_iter().next())
        .unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let client = DistillClient::new(&server_url);
    if client.health().await.is_err() {
        tracing::debug!("Skipping auto-index: distill server not reachable at {server_url}");
        return false;
    }

    // Spawn index daemon with defaults (no specific files, no force)
    match spawn_index_daemon(&None, None, false, false) {
        Ok(pid) => {
            tracing::info!("Auto-started index daemon (PID {pid})");
            true
        }
        Err(e) => {
            tracing::warn!("Failed to auto-start index daemon: {e}");
            false
        }
    }
}

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
    let servers = resolve_servers(server).await;
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

const INDEX_STATUS_FILE: &str = "distill-index-status.json";

fn index_pid_path() -> PathBuf {
    hidden_dir().join("distill-index.pid")
}

fn index_status_path() -> PathBuf {
    hidden_dir().join(INDEX_STATUS_FILE)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct IndexStatus {
    pid: u32,
    total_files: usize,
    indexed: usize,
    failed: usize,
    gpu_yield: bool,
    total_chunks: u32,
    current_file: String,
    done: bool,
}

fn read_index_status() -> Option<IndexStatus> {
    let contents = std::fs::read_to_string(index_status_path()).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_index_status(status: &IndexStatus) {
    if let Ok(json) = serde_json::to_string(status) {
        let _ = std::fs::write(index_status_path(), json);
    }
}

/// Spawn the index daemon as a background process.
fn spawn_index_daemon(
    files: &Option<Vec<PathBuf>>,
    server: Option<&str>,
    no_yield: bool,
    force: bool,
) -> Result<u32> {
    let exe = std::env::current_exe().context("Cannot find current executable")?;

    let mut args = vec![
        "distill".to_string(),
        "index".to_string(),
        "--daemon-child".to_string(),
    ];
    if no_yield {
        args.push("--no-yield".to_string());
    }
    if force {
        args.push("--force".to_string());
    }
    if let Some(s) = server {
        args.push("--server".to_string());
        args.push(s.to_string());
    }
    if let Some(file_list) = files {
        for f in file_list {
            args.push("--file".to_string());
            args.push(f.to_string_lossy().to_string());
        }
    }

    let log_path = hs_common::resolve_log_dir().join("distill-index.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Cannot open daemon log file")?;
    let log_err = log_file.try_clone()?;

    let child = std::process::Command::new(exe)
        .args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn index daemon")?;

    Ok(child.id())
}

/// Foreground: spawn daemon, then attach to its progress.
async fn cmd_index(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    no_yield: bool,
    force: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    // Check if daemon already running
    let pid_path = index_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            reporter.status(
                "Index",
                &format!("already running (PID {pid}). Attaching..."),
            );
            return attach_index(reporter).await;
        }
        crate::daemon::remove_pid_file(&pid_path);
    }

    // Health check before spawning
    let servers = resolve_servers(server).await;
    let client = DistillClient::new(&servers[0]);
    match client.health().await {
        Ok(h) => reporter.status(
            "Connected",
            &format!("{} ({})", servers[0], h.compute_device),
        ),
        Err(e) => {
            return Err(e).context(format!("Is hs-distill-server running at {}?", servers[0]));
        }
    };

    // Spawn daemon
    let pid = spawn_index_daemon(&files, server, no_yield, force)?;
    reporter.status(
        "Index",
        &format!("daemon started (PID {pid}). Press q to detach."),
    );

    // Wait briefly for status file to appear
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    attach_index(reporter).await
}

/// Attach to a running index daemon — display progress, q to detach.
async fn attach_index(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let raw_enabled = crossterm::terminal::enable_raw_mode().is_ok();
    let mut last_indexed = 0usize;

    loop {
        // Poll for keypress with short timeout (stays responsive)
        if raw_enabled {
            if crossterm::event::poll(std::time::Duration::from_millis(200)).unwrap_or(false) {
                if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                    if key.kind == crossterm::event::KeyEventKind::Press
                        && matches!(
                            key.code,
                            crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                        )
                    {
                        let _ = crossterm::terminal::disable_raw_mode();
                        reporter.status("Index", "detached. Daemon continues in background.");
                        return Ok(());
                    }
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Read status
        if let Some(status) = read_index_status() {
            if status.indexed > last_indexed {
                let _ = crossterm::terminal::disable_raw_mode();
                eprintln!(
                    "  [{}/{}] {} — {} chunks total",
                    status.indexed, status.total_files, status.current_file, status.total_chunks
                );
                if raw_enabled {
                    let _ = crossterm::terminal::enable_raw_mode();
                }
                last_indexed = status.indexed;
            } else if status.gpu_yield {
                let _ = crossterm::terminal::disable_raw_mode();
                eprint!(
                    "\r  [{}/{}] yielding to scribe...   ",
                    status.indexed, status.total_files
                );
                if raw_enabled {
                    let _ = crossterm::terminal::enable_raw_mode();
                }
            }

            if status.done {
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.finish(&format!(
                    "Indexed {}/{} files, {} chunks ({} failed)",
                    status.indexed, status.total_files, status.total_chunks, status.failed
                ));
                let _ = std::fs::remove_file(index_status_path());
                crate::daemon::remove_pid_file(&index_pid_path());
                return Ok(());
            }

            if !crate::daemon::is_process_alive(status.pid) {
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.error("Index daemon exited unexpectedly. Check logs.");
                return Ok(());
            }
        }
    }
}

/// Daemon child: run the actual indexing loop, write status file.
async fn cmd_index_daemon(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    no_yield: bool,
    force: bool,
) -> Result<()> {
    // Write PID
    let pid_path = index_pid_path();
    crate::daemon::write_pid_file(&pid_path)?;

    let servers = resolve_servers(server).await;
    let client = DistillClient::new(&servers[0]);

    // Health check
    client
        .health()
        .await
        .context(format!("Is hs-distill-server running at {}?", servers[0]))?;

    // Determine files
    let config = DistillClientConfig::load().unwrap_or_default();
    let catalog_dir = config.catalog_dir.clone();
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

    let mut status = IndexStatus {
        pid: std::process::id(),
        total_files: paths.len(),
        ..Default::default()
    };
    write_index_status(&status);

    for path in &paths {
        // Yield GPU to scribe if it has work queued
        if !no_yield && hs_common::gpu_priority::scribe_is_active() {
            status.gpu_yield = true;
            write_index_status(&status);
            tracing::info!("Yielding to scribe (has active work)");
            hs_common::gpu_priority::wait_for_scribe_idle().await;
            status.gpu_yield = false;
            tracing::info!("Scribe idle, resuming indexing");
        }

        let path_str = path.to_string_lossy().to_string();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        status.current_file = stem.to_string();
        write_index_status(&status);

        // Skip if already indexed (unless --force)
        if !force && client.doc_exists(stem).await.unwrap_or(false) {
            status.indexed += 1;
            write_index_status(&status);
            continue;
        }

        match client.index_file_with_progress(&path_str, |_| {}).await {
            Ok(result) => {
                status.total_chunks += result.chunks_indexed;
                status.indexed += 1;
                hs_common::catalog::update_embedding_catalog(
                    &catalog_dir,
                    stem,
                    &servers[0],
                    result.chunks_indexed,
                    &result.embedding_device,
                );
            }
            Err(e) => {
                status.failed += 1;
                tracing::error!("{stem}: {e}");
            }
        }
        write_index_status(&status);
    }

    status.done = true;
    write_index_status(&status);

    crate::daemon::remove_pid_file(&pid_path);
    Ok(())
}

// ── Search ──────────────────────────────────────────────────────

async fn cmd_search(
    query: &str,
    limit: u64,
    year: Option<String>,
    topic: Option<String>,
    server: Option<&str>,
    global: &GlobalArgs,
) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("Search query cannot be empty");
    }

    let servers = resolve_servers(server).await;
    let client = make_distill_client(&servers[0]).await?;

    let filters = hs_distill::client::SearchFilters { year, topic };
    let hits = client
        .search(query, limit, filters)
        .await
        .context(format!("Is hs-distill-server running at {}?", servers[0]))?;

    match global.output {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&hits)?;
            println!("{json}");
        }
        OutputFormat::Ndjson => {
            for hit in &hits {
                let line = serde_json::to_string(hit)?;
                println!("{line}");
            }
        }
        OutputFormat::Text => {
            if hits.is_empty() {
                println!("No results found.");
                return Ok(());
            }

            for (i, hit) in hits.iter().enumerate() {
                let title = hit.title.as_deref().unwrap_or(&hit.doc_id);
                let authors = if hit.authors.is_empty() {
                    String::new()
                } else {
                    format!(" by {}", hit.authors.join(", "))
                };
                let year_str = hit.year.map(|y| format!(" ({})", y)).unwrap_or_default();
                let page_info = hit
                    .page
                    .map(|p| format!(" (page {})", p))
                    .unwrap_or_default();
                let pdf = hit.pdf_path.as_deref().unwrap_or("?");

                println!(
                    "\n{}. {}{} [score: {:.3}]{}",
                    i + 1,
                    title,
                    authors,
                    hit.score,
                    year_str
                );
                println!(
                    "   {}:{}-{}{}",
                    hit.doc_id, hit.line_start, hit.line_end, page_info
                );
                println!("   PDF: {pdf}");

                let preview: String = hit.chunk_text.chars().take(200).collect();
                println!("   {preview}...");
            }
        }
    }

    Ok(())
}
