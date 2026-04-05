use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::compose::ComposeCmd;
use hs_common::global_args::GlobalArgs;
use hs_common::reporter::Reporter;

const GITHUB_API_LATEST: &str = "https://api.github.com/repos/home-still/home/releases/latest";

// ── GitHub API types ────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

// ── Entry point ─────────────────────────────────────────────────

pub async fn run(
    check_only: bool,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let current = current_version();
    reporter.status("Current", &format!("hs {current}"));

    // Phase 1: fetch latest release
    let release = fetch_latest_release(reporter).await?;
    let latest = parse_release_version(&release.tag_name)?;

    if latest <= current {
        reporter.finish(&format!("Already up to date ({current})"));
        return Ok(());
    }

    reporter.status("Available", &format!("{current} → {latest}"));

    if check_only {
        return Ok(());
    }

    // Phase 2: confirm
    if !global.yes {
        let proceed = dialoguer::Confirm::new()
            .with_prompt(format!("Upgrade hs from {current} to {latest}?"))
            .default(true)
            .interact()?;
        if !proceed {
            reporter.status("Skipped", "upgrade cancelled");
            return Ok(());
        }
    }

    // Phase 3: download and replace binaries
    let target = detect_target()?;
    reporter.status("Platform", target);

    let hs_installed = download_and_replace_binary(&release, "hs", target, reporter).await?;

    if hs_installed {
        reporter.status("Upgraded", &format!("hs → {latest}"));
    }

    // Also upgrade hs-distill-server if it's already installed
    if find_distill_binary().is_some() {
        let distill_installed =
            download_and_replace_binary(&release, "hs-distill-server", target, reporter).await?;
        if distill_installed {
            reporter.status("Upgraded", &format!("hs-distill-server → {latest}"));
        } else {
            reporter.status(
                "Skipped",
                "hs-distill-server (no release asset for this platform)",
            );
        }
    }

    // Phase 4: update Docker services
    upgrade_docker_services(reporter).await?;

    // Phase 5: health check
    post_upgrade_health_check(reporter).await;

    reporter.finish(&format!(
        "Upgraded to {latest}. Run `hs status` for full dashboard."
    ));
    Ok(())
}

// ── Version helpers ─────────────────────────────────────────────

fn current_version() -> semver::Version {
    let raw = env!("HS_VERSION");
    // Try parsing as-is first (works for CI builds: "0.0.1-rc.99")
    if let Ok(v) = semver::Version::parse(raw) {
        return v;
    }
    // git describe produces e.g. "0.0.1-rc.99-3-gabcdef" — try progressively
    // shorter suffixes until we find valid semver.
    let mut candidate = raw.to_string();
    while let Some(pos) = candidate.rfind('-') {
        candidate.truncate(pos);
        if let Ok(v) = semver::Version::parse(&candidate) {
            return v;
        }
    }
    semver::Version::new(0, 0, 0)
}

fn parse_release_version(tag: &str) -> Result<semver::Version> {
    let raw = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(raw).context("invalid version in release tag")
}

// ── Platform detection (compile-time) ───────��───────────────────

fn detect_target() -> Result<&'static str> {
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        return Ok("x86_64-apple-darwin");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        return Ok("aarch64-apple-darwin");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        return Ok("x86_64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        return Ok("aarch64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        return Ok("x86_64-pc-windows-msvc");
    }
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!("Unsupported platform for self-update"))
}

// ── GitHub API ──────────────────────────────────────────────────

async fn fetch_latest_release(reporter: &Arc<dyn Reporter>) -> Result<GitHubRelease> {
    reporter.status("Checking", "GitHub for latest release...");

    let mut builder = reqwest::Client::builder()
        .user_agent(format!("hs/{}", env!("HS_VERSION")))
        .build()?
        .get(GITHUB_API_LATEST);

    // Support GITHUB_TOKEN for rate-limited environments
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        builder = builder.bearer_auth(token);
    }

    let resp = builder.send().await.context("Failed to reach GitHub API")?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("GitHub API rate limit exceeded. Set GITHUB_TOKEN env var to authenticate.");
    }
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }

    resp.json().await.context("Failed to parse release JSON")
}

// ── Binary download & replacement ───────────────────────────────

async fn download_and_replace_binary(
    release: &GitHubRelease,
    binary_name: &str,
    target: &str,
    reporter: &Arc<dyn Reporter>,
) -> Result<bool> {
    let archive_name = format!("{binary_name}-{}-{target}.tar.gz", release.tag_name);

    let asset = match release.assets.iter().find(|a| a.name == archive_name) {
        Some(a) => a,
        None => return Ok(false), // no asset for this platform
    };

    reporter.status("Downloading", &archive_name);

    let resp = reqwest::get(&asset.browser_download_url)
        .await
        .context("Download failed")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "Download failed ({}): {}",
            resp.status(),
            asset.browser_download_url
        );
    }

    let bytes = resp.bytes().await.context("Failed to read download")?;

    // Extract binary from tar.gz
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);

    let mut binary_data: Option<Vec<u8>> = None;
    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry?;
        let path = entry.path()?;
        let file_name = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        if file_name == binary_name {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            binary_data = Some(data);
            break;
        }
    }

    let binary_data = binary_data
        .ok_or_else(|| anyhow::anyhow!("Binary '{binary_name}' not found in archive"))?;

    // Determine install location
    let install_path = install_path_for(binary_name)?;
    let install_dir = install_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid install path"))?;

    // Write to temp file, then atomic rename
    let tmp_path = install_dir.join(format!(".{binary_name}.upgrade.tmp"));

    std::fs::write(&tmp_path, &binary_data)
        .with_context(|| format!("Failed to write to {}", tmp_path.display()))?;

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Atomic rename
    std::fs::rename(&tmp_path, &install_path).with_context(|| {
        format!(
            "Failed to replace {}. Check permissions on {}",
            install_path.display(),
            install_dir.display()
        )
    })?;

    Ok(true)
}

fn install_path_for(binary_name: &str) -> Result<PathBuf> {
    if binary_name == "hs" {
        std::env::current_exe().context("Could not determine current executable path")
    } else {
        // For hs-distill-server, use the same location where it's currently installed
        find_distill_binary()
            .ok_or_else(|| anyhow::anyhow!("{binary_name} not found on this system"))
    }
}

fn find_distill_binary() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-distill-server");
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-distill-server");
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

// ── Docker service upgrade ──────────────────────────────────────

fn hidden_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
}

async fn upgrade_docker_services(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_compose = hidden_dir().join("docker-compose.yml");
    let distill_compose = hidden_dir().join("docker-compose-distill.yml");

    let has_scribe = scribe_compose.exists();
    let has_distill = distill_compose.exists();

    if !has_scribe && !has_distill {
        reporter.status("Skipped", "no Docker services on this host");
        return Ok(());
    }

    let compose = ComposeCmd::detect().await;
    let compose = match compose {
        Some(c) => c,
        None => {
            reporter.warn("Docker compose not found — skipping container updates");
            return Ok(());
        }
    };

    let compose_files: Vec<(&Path, &str)> = [
        (scribe_compose.as_path(), "scribe"),
        (distill_compose.as_path(), "distill"),
    ]
    .into_iter()
    .filter(|(p, _)| p.exists())
    .collect();

    for (cf, name) in &compose_files {
        let cf_str = cf.to_str().unwrap_or_default();
        reporter.status("Pulling", &format!("new images for {name}..."));
        let pull = compose.run(&["-f", cf_str, "pull"]).await?;
        if !pull.success() {
            reporter.warn(&format!("Failed to pull images for {name}"));
            continue;
        }

        reporter.status("Restarting", &format!("{name} containers..."));
        let up = compose.run(&["-f", cf_str, "up", "-d"]).await?;
        if !up.success() {
            reporter.warn(&format!("Failed to restart {name} containers"));
        }
    }

    Ok(())
}

// ── Post-upgrade health check ───────────────────────────────────

async fn post_upgrade_health_check(reporter: &Arc<dyn Reporter>) {
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Give containers a moment to start
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let scribe_compose = hidden_dir().join("docker-compose.yml");
    if scribe_compose.exists() {
        match http.get("http://localhost:7433/health").send().await {
            Ok(resp) if resp.status().is_success() => {
                reporter.status("Health", "scribe: OK");
            }
            _ => {
                reporter.warn("scribe: not responding (may still be starting)");
            }
        }
    }

    let distill_compose = hidden_dir().join("docker-compose-distill.yml");
    if distill_compose.exists() {
        match http.get("http://localhost:7434/health").send().await {
            Ok(resp) if resp.status().is_success() => {
                reporter.status("Health", "distill: OK");
            }
            _ => {
                reporter.warn("distill: not responding (may still be starting)");
            }
        }
    }
}
