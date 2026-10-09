use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::compose::ComposeCmd;
use hs_common::global_args::GlobalArgs;
use hs_common::reporter::Reporter;

use crate::distill_cmd::compose_step;
use crate::installer::{Installer, Prepared, Release};

/// How long the upgraded services get to answer `/health`.
const HEALTH_WAIT_SECS: u64 = 90;

/// How long a restarted native distill server gets to start answering
/// `/health` before the CUDA check is skipped as "unit stopped".
const DISTILL_HEALTH_WAIT: std::time::Duration = std::time::Duration::from_secs(HEALTH_WAIT_SECS);

/// Companion binaries upgraded when already installed on this host. Each
/// name is a release-asset prefix (see `.github/workflows/release.yaml`).
const COMPANIONS: [&str; 4] = [
    "hs-distill-server",
    "hs-gateway",
    "hs-mcp",
    "hs-scribe-server",
];

// ── Entry point ─────────────────────────────────────────────────

pub async fn run(
    check_only: bool,
    force: bool,
    include_pre: bool,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let current = current_version();
    reporter.status("Current", &format!("hs {current}"));

    // Phase 1: fetch latest release (including pre-releases if --pre)
    let target = crate::installer::detect_target()?;
    let installer = Installer::new(crate::installer::DEFAULT_API_BASE, target)?;
    reporter.status(
        "Checking",
        if include_pre {
            "GitHub for latest release (including pre-releases)..."
        } else {
            "GitHub for latest release..."
        },
    );
    let release = if include_pre {
        installer.latest_release_including_pre().await?
    } else {
        installer.latest_release().await?
    };
    let latest = parse_release_version(&release.tag_name)?;

    if latest <= current && !force {
        reporter.finish(&format!("Already up to date ({current})"));
        return Ok(());
    }

    if force && latest <= current {
        reporter.status("Force", &format!("reinstalling {latest}"));
    } else {
        reporter.status("Available", &format!("{current} → {latest}"));
    }

    if check_only {
        return Ok(());
    }

    // Phase 2: confirm
    if !global.yes {
        let prompt = if force && latest <= current {
            format!("Force reinstall hs {latest}?")
        } else {
            format!("Upgrade hs from {current} to {latest}?")
        };
        let proceed = dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(true)
            .interact()?;
        if !proceed {
            reporter.status("Skipped", "upgrade cancelled");
            return Ok(());
        }
    }

    // Phase 3: resolve every asset (and its checksum) before changing
    // anything, so a missing asset aborts with the host untouched.
    reporter.status("Platform", target);
    let plan = plan_installs(&installer, &release, reporter).await?;

    // Phase 4: update Docker services — BEFORE any binary is replaced. This
    // phase is the failure-prone one (it pulls images over the network and
    // recreates containers) and does not depend on the new binaries. Run
    // after the swap, a failure here stranded the host on new binaries with
    // old processes still running, and the rerun then reported "Already up
    // to date" (the new `hs` is installed) without ever restarting them.
    // Run first, a failure leaves the old `hs` in place and a plain
    // `hs upgrade` retries everything.
    upgrade_docker_services(reporter).await?;

    // Phase 5: download and replace the binaries, `hs` last (see
    // `plan_installs`). The replaced set drives the restart phase, not a
    // fixed list of service names.
    let mut replaced: Vec<PathBuf> = Vec::new();
    for (prepared, path) in &plan.installs {
        installer.install(prepared, path, reporter).await?;
        reporter.status("Upgraded", &format!("{} → {latest}", prepared.binary));
        replaced.push(path.clone());
    }
    if !plan.skipped.is_empty() {
        reporter.warn(&format!(
            "not upgraded (no release asset for {target}): {}",
            plan.skipped.join(", ")
        ));
    }

    // Phase 6: restart the services running the binaries we replaced, and
    // prove they came back on the new binary. A failure here fails the
    // upgrade — the new binaries are on disk but not running.
    reporter.status(
        "Restart",
        "restarting services running the replaced binaries...",
    );
    crate::restart_cmd::after_upgrade(&replaced, reporter).await?;

    // Phase 7: health check
    post_upgrade_health_check(reporter).await?;

    let tail = if plan.skipped.is_empty() {
        String::new()
    } else {
        format!(
            " Not upgraded (no asset for {target}): {}.",
            plan.skipped.join(", ")
        )
    };
    reporter.finish(&format!(
        "Upgraded to {latest}. Run `hs status` for full dashboard.{tail}"
    ));
    Ok(())
}

struct InstallPlan {
    /// Resolved, checksum-verified-in-advance binaries and where they go.
    installs: Vec<(Prepared, PathBuf)>,
    /// Installed companions the release publishes no asset for on this platform.
    skipped: Vec<&'static str>,
}

/// `hs` is mandatory (missing asset = Err); a companion is planned only when
/// installed on this host, and skipped loudly when the release has no asset.
async fn plan_installs(
    installer: &Installer,
    release: &Release,
    reporter: &Arc<dyn Reporter>,
) -> Result<InstallPlan> {
    // `hs` is installed LAST. The release comparison in `run` is against the
    // running `hs`: if it were replaced first and a companion install then
    // failed, the next `hs upgrade` would report "Already up to date" and the
    // companion would never be upgraded. With `hs` last, a failed run leaves
    // the old `hs` in place and a rerun retries everything.
    let hs = installer.prepare_required(release, "hs").await?;
    let mut installs = Vec::new();
    let mut skipped = Vec::new();
    for name in COMPANIONS {
        let Some(path) = find_companion_binary(name) else {
            continue;
        };
        match installer.prepare(release, name).await? {
            Some(p) => installs.push((p, path)),
            None => {
                reporter.warn(&format!(
                    "{name} is installed but the release has no asset for {}; leaving it at its current version",
                    installer.target()
                ));
                skipped.push(name);
            }
        }
    }
    installs.push((hs, install_path_for("hs")?));
    Ok(InstallPlan { installs, skipped })
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

fn install_path_for(binary_name: &str) -> Result<PathBuf> {
    if binary_name == "hs" {
        std::env::current_exe().context("Could not determine current executable path")
    } else {
        find_companion_binary(binary_name)
            .ok_or_else(|| anyhow::anyhow!("{binary_name} not found on this system"))
    }
}

/// Find a companion binary (hs-distill-server, hs-gateway, hs-mcp) on disk.
pub(crate) fn find_companion_binary(name: &str) -> Option<PathBuf> {
    // Check ~/.local/bin
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin").join(name);
        if path.exists() {
            return Some(path);
        }
    }
    // Check next to the current hs binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

// ── Docker service upgrade ──────────────────────────────────────

async fn upgrade_docker_services(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load()?;
    let hidden = hs_common::hidden_dir()?;
    let scribe_compose = hidden.join("docker-compose.yml");
    let distill_compose = hidden.join("docker-compose-distill.yml");

    let has_scribe = scribe_cfg.local_server && scribe_compose.exists();
    let has_distill = distill_compose.exists();

    if !has_scribe && !has_distill {
        reporter.status("Skipped", "no Docker services on this host");
        return Ok(());
    }

    let compose = ComposeCmd::detect().await.ok_or_else(|| {
        anyhow::anyhow!(
            "Docker services are configured on this host but no compose runtime was found"
        )
    })?;

    let compose_files: Vec<(&Path, &str)> = [
        (scribe_compose.as_path(), "scribe"),
        (distill_compose.as_path(), "distill"),
    ]
    .into_iter()
    .filter(|(p, _)| p.exists())
    .collect();

    for (cf, name) in compose_files {
        upgrade_compose_service(&compose, cf, name, reporter).await?;
    }
    Ok(())
}

/// pull → down → up -d for one compose file; any failing step fails the
/// upgrade. (`down` first avoids podman pod conflicts on recreate.)
async fn upgrade_compose_service(
    compose: &ComposeCmd,
    compose_file: &Path,
    name: &str,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let cf = compose_file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 compose path {}", compose_file.display()))?;
    reporter.status("Pulling", &format!("new images for {name}..."));
    compose_step(compose, &["-f", cf, "pull"])
        .await
        .with_context(|| format!("upgrading {name} containers"))?;
    reporter.status("Stopping", &format!("{name} containers..."));
    compose_step(compose, &["-f", cf, "down"])
        .await
        .with_context(|| format!("upgrading {name} containers"))?;
    reporter.status("Starting", &format!("{name} containers..."));
    compose_step(compose, &["-f", cf, "up", "-d"])
        .await
        .with_context(|| format!("upgrading {name} containers"))
}

// ── Post-upgrade health check ───────────────────────────────────

/// The base URL of the service this host runs locally, taken from the
/// configured server list: the first entry whose host is loopback.
fn local_service_url<'a>(urls: impl IntoIterator<Item = &'a str>, service: &str) -> Result<String> {
    let mut seen = Vec::new();
    for raw in urls {
        let url = reqwest::Url::parse(raw)
            .with_context(|| format!("invalid {service} server URL `{raw}` in config"))?;
        if matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        ) {
            return Ok(raw.trim_end_matches('/').to_string());
        }
        seen.push(raw.to_string());
    }
    anyhow::bail!(
        "{service} runs in Docker on this host but no loopback {service} server is configured (configured: {})",
        if seen.is_empty() { "none".to_string() } else { seen.join(", ") }
    )
}

async fn post_upgrade_health_check(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load()?;
    let hidden = hs_common::hidden_dir()?;
    let scribe_compose = hidden.join("docker-compose.yml");
    if scribe_cfg.local_server && scribe_compose.exists() {
        let url = local_service_url(scribe_cfg.servers.iter().map(|s| s.url.as_str()), "scribe")?;
        hs_common::compose::wait_for_url(&format!("{url}/health"), HEALTH_WAIT_SECS, "scribe")
            .await?;
        reporter.status("Health", "scribe: OK");
    }

    if hidden.join("docker-compose-distill.yml").exists() {
        // That compose file (written by `hs distill init`) runs Qdrant only;
        // the distill server is always a native unit, checked below.
        let qdrant = crate::distill_cmd::qdrant_rest_url()?;
        hs_common::compose::wait_for_url(&format!("{qdrant}/healthz"), HEALTH_WAIT_SECS, "Qdrant")
            .await?;
        reporter.status("Health", "Qdrant: OK");
    }
    if find_companion_binary("hs-distill-server").is_some() {
        let cfg = hs_distill::config::DistillClientConfig::load()
            .map_err(|e| anyhow::anyhow!("distill config: {e}"))?;
        verify_native_distill(&cfg.servers, reporter, DISTILL_HEALTH_WAIT).await?;
    }
    Ok(())
}

/// After an upgrade of a natively installed distill server: a local distill
/// that is answering MUST be on CUDA (never CPU, never flipped here). The
/// restarted server needs time to load its models before it listens, so
/// `/health` is polled for `wait`. If it never answers, the unit is stopped —
/// the restart phase has already failed the upgrade for a unit that was
/// running and did not come back — so there is nothing to assert; say so
/// rather than pretend it was verified.
async fn verify_native_distill(
    servers: &[String],
    reporter: &Arc<dyn Reporter>,
    wait: std::time::Duration,
) -> Result<()> {
    let Ok(url) = local_service_url(servers.iter().map(String::as_str), "distill") else {
        reporter.status(
            "Health",
            "distill: no loopback server configured, CUDA not checked",
        );
        return Ok(());
    };
    let client = hs_distill::client::DistillClient::new(&url)?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match client.health().await {
            Ok(health) if health.compute_device.eq_ignore_ascii_case("cuda") => {
                reporter.status("Health", "distill: OK (cuda)");
                return Ok(());
            }
            Ok(health) => anyhow::bail!(
                "distill at {url} reports compute_device `{}`, expected cuda",
                health.compute_device
            ),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                reporter.warn(&format!(
                    "distill at {url} is not answering ({e:#}): its unit is stopped, CUDA was NOT verified"
                ));
                return Ok(());
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::installer::test_support::{serve, Route};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn reporter() -> Arc<dyn Reporter> {
        Arc::new(hs_common::reporter::SilentReporter)
    }

    /// Stand-in compose binary: logs argv, exits 1 for the subcommand in `$FAIL`.
    fn fake_compose(dir: &Path, fail_on: &str) -> ComposeCmd {
        let bin = dir.join("fake-compose");
        let log = dir.join("calls.log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$3\" = \"{fail_on}\" ]; then echo boom >&2; exit 1; fi\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        ComposeCmd {
            bin: bin.to_string_lossy().into_owned(),
            args_prefix: vec![],
        }
    }

    #[tokio::test]
    async fn compose_down_failure_fails_the_upgrade_step() {
        let dir = tempfile::tempdir().unwrap();
        let compose = fake_compose(dir.path(), "down");
        let cf = dir.path().join("docker-compose.yml");
        std::fs::write(&cf, "services: {}\n").unwrap();
        let err = upgrade_compose_service(&compose, &cf, "scribe", &reporter())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("down"), "{err:#}");
        // `up -d` must not run after a failed `down`
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(!log.contains("up -d"), "{log}");
    }

    #[tokio::test]
    async fn compose_pull_and_up_failures_fail_too_and_success_runs_all_three() {
        for fail in ["pull", "up"] {
            let dir = tempfile::tempdir().unwrap();
            let compose = fake_compose(dir.path(), fail);
            let cf = dir.path().join("c.yml");
            std::fs::write(&cf, "").unwrap();
            assert!(
                upgrade_compose_service(&compose, &cf, "distill", &reporter())
                    .await
                    .is_err()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let compose = fake_compose(dir.path(), "none");
        let cf = dir.path().join("c.yml");
        std::fs::write(&cf, "").unwrap();
        upgrade_compose_service(&compose, &cf, "distill", &reporter())
            .await
            .unwrap();
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let verbs: Vec<&str> = log.lines().map(|l| l.split(' ').nth(2).unwrap()).collect();
        assert_eq!(verbs, ["pull", "down", "up"]);
    }

    fn health_route(device: &str) -> HashMap<String, Route> {
        HashMap::from([(
            "/health".to_string(),
            Route::ok(format!(
                r#"{{"status":"ok","compute_device":"{device}","collection":"c"}}"#
            )),
        )])
    }

    #[test]
    fn health_urls_come_from_config_not_hardcoded_ports() {
        let url = local_service_url(
            [
                "http://scribe-1.example.local:7433",
                "http://localhost:9911/",
            ],
            "scribe",
        )
        .unwrap();
        assert_eq!(url, "http://localhost:9911");
        assert!(local_service_url(["http://scribe-1.example.local:7433"], "scribe").is_err());
        assert!(local_service_url([], "distill").is_err());
    }

    #[tokio::test]
    async fn a_native_distill_that_answers_must_be_on_cuda() {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
        let cpu = serve(health_route("Cpu")).await;
        let err = verify_native_distill(&[cpu], &reporter(), WAIT)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("cuda"), "{err:#}");

        let cuda = serve(health_route("Cuda")).await;
        verify_native_distill(&[cuda], &reporter(), WAIT)
            .await
            .unwrap();

        // Stopped unit: nothing to assert (the restart phase owns that failure).
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        verify_native_distill(&[dead], &reporter(), std::time::Duration::ZERO)
            .await
            .unwrap();
    }
}
