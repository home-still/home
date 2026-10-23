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
    describe_base_version(env!("HS_VERSION"))
}

/// The release a build is based on. A tagged build's `HS_VERSION` is the tag
/// (`0.0.1-rc.99`); a development build's is `git describe` output
/// (`0.0.1-rc.99-3-gabcdef`). That parses as semver too, with the commit count
/// and hash glued onto the last pre-release identifier, which makes it
/// alphanumeric and so *newer* than every later rc (`rc.360`): the stale
/// development build would be told it is up to date. A describe with no tag
/// at all (a bare hash) is based on no release: `0.0.0`, older than all of them.
fn describe_base_version(raw: &str) -> semver::Version {
    semver::Version::parse(strip_describe_suffix(raw))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

/// `<tag>-<commits>-g<hash>` -> `<tag>`; anything else is returned unchanged.
fn strip_describe_suffix(raw: &str) -> &str {
    let Some((rest, hash)) = raw.rsplit_once('-') else {
        return raw;
    };
    let Some((tag, commits)) = rest.rsplit_once('-') else {
        return raw;
    };
    let is_hash = hash
        .strip_prefix('g')
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    let is_count = !commits.is_empty() && commits.bytes().all(|b| b.is_ascii_digit());
    if is_hash && is_count {
        tag
    } else {
        raw
    }
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
    let files = crate::restart_cmd::managed_compose_files(&hidden, scribe_cfg.local_server);

    if files.is_empty() {
        reporter.status("Skipped", "no Docker services on this host");
        return Ok(());
    }

    let compose = crate::restart_cmd::compose_runtime_for(&files).await?;

    for (name, cf) in &files {
        let health = compose_health_url(name, &scribe_cfg)?;
        upgrade_compose_service(
            &compose,
            compose.engine(),
            cf,
            name,
            health.as_deref(),
            reporter,
        )
        .await?;
    }
    Ok(())
}

/// The URL that answers 200 when a managed compose service is up: the one
/// the post-upgrade health check waits on. `None` for a file this host has
/// no health probe for.
fn compose_health_url(
    name: &str,
    scribe_cfg: &hs_scribe::config::ScribeConfig,
) -> Result<Option<String>> {
    Ok(match name {
        "scribe" => Some(format!(
            "{}/health",
            local_service_url(scribe_cfg.servers.iter().map(|s| s.url.as_str()), "scribe")?
        )),
        // That compose file (written by `hs distill init`) runs Qdrant only.
        "distill" => Some(format!(
            "{}/healthz",
            crate::distill_cmd::qdrant_rest_url()?
        )),
        _ => None,
    })
}

/// Pull one compose file's images, then recreate its containers (`down` →
/// `up -d`) only if something changed; any failing step fails the upgrade.
/// (`down` first avoids podman pod conflicts on recreate.)
///
/// Recreating is not free: `down` stops the service. For Qdrant that is a
/// search outage until it reloads its collections — 45–67 s measured on a
/// copy of the corpus, ~7 minutes in production on 2026-10-22, longer than
/// the post-upgrade health wait. So the containers are left running when
/// each already runs the image its service names after the pull and the
/// service answers its health URL. Anything that cannot be established —
/// no engine, a probe failing, a service without an `image` — recreates,
/// as every upgrade did before.
async fn upgrade_compose_service(
    compose: &ComposeCmd,
    engine: Option<&str>,
    compose_file: &Path,
    name: &str,
    health_url: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let cf = compose_file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 compose path {}", compose_file.display()))?;
    reporter.status("Pulling", &format!("new images for {name}..."));
    compose_step(compose, &["-f", cf, "pull"])
        .await
        .with_context(|| format!("upgrading {name} containers"))?;

    let unchanged = match (engine, health_url) {
        (Some(engine), Some(url)) => match probe_service_images(compose, engine, cf).await {
            Ok(images) => images.unchanged() && answers_200(url).await,
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "{name}: cannot compare running and pulled images; recreating");
                false
            }
        },
        _ => false,
    };
    if unchanged {
        reporter.status(
            "Unchanged",
            &format!("{name}: images unchanged and healthy; containers left running"),
        );
        return Ok(());
    }

    reporter.status("Stopping", &format!("{name} containers..."));
    compose_step(compose, &["-f", cf, "down"])
        .await
        .with_context(|| format!("upgrading {name} containers"))?;
    reporter.status("Starting", &format!("{name} containers..."));
    compose_step(compose, &["-f", cf, "up", "-d"])
        .await
        .with_context(|| format!("upgrading {name} containers"))
}

/// A compose project's images after a pull, against what its containers run.
#[derive(Debug, Default, PartialEq)]
struct ServiceImages {
    /// Services the compose file defines.
    services: usize,
    /// The image id each service's `image` names now.
    named: Vec<String>,
    /// The image id each running container of the project runs.
    running: Vec<String>,
}

impl ServiceImages {
    /// Every service has one running container, and the containers run
    /// exactly the images the services name.
    fn unchanged(&self) -> bool {
        let norm = |id: &String| id.trim().trim_start_matches("sha256:").to_string();
        let named: std::collections::BTreeSet<String> = self.named.iter().map(norm).collect();
        let running: std::collections::BTreeSet<String> = self.running.iter().map(norm).collect();
        self.services > 0
            && self.named.len() == self.services
            && self.running.len() == self.services
            && named == running
    }
}

/// Ask the compose runtime and its engine what the project's services name
/// and what its containers run. Any command failing is an error (the
/// caller recreates).
async fn probe_service_images(
    compose: &ComposeCmd,
    engine: &str,
    cf: &str,
) -> Result<ServiceImages> {
    let config = capture(
        compose.run_capture(&["-f", cf, "config"]).await,
        "compose config",
    )?;
    let images = service_images(&config)?;
    let mut named = Vec::with_capacity(images.len());
    for image in &images {
        named
            .push(engine_query(engine, &["image", "inspect", "--format", "{{.Id}}", image]).await?);
    }
    let ids = capture(
        compose.run_capture(&["-f", cf, "ps", "-q"]).await,
        "compose ps -q",
    )?;
    let mut running = Vec::new();
    for id in ids.split_whitespace() {
        running.push(engine_query(engine, &["inspect", "--format", "{{.Image}}", id]).await?);
    }
    Ok(ServiceImages {
        services: images.len(),
        named,
        running,
    })
}

/// The `image` of every service in resolved compose YAML. A service with
/// no `image` (built locally) cannot be compared, so it is an error.
fn service_images(config_yaml: &str) -> Result<Vec<String>> {
    let doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(config_yaml).context("parsing `compose config` output")?;
    let services = doc
        .get("services")
        .and_then(|s| s.as_mapping())
        .context("`compose config` output has no services")?;
    services
        .iter()
        .map(|(name, svc)| {
            svc.get("image")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .with_context(|| format!("service {name:?} names no image"))
        })
        .collect()
}

fn capture(out: Result<std::process::Output>, what: &str) -> Result<String> {
    let out = out.with_context(|| format!("running {what}"))?;
    anyhow::ensure!(out.status.success(), "{what} exited {}", out.status);
    String::from_utf8(out.stdout).with_context(|| format!("{what} printed non-UTF-8"))
}

async fn engine_query(engine: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(engine)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(anyhow::Error::from);
    let what = format!("{engine} {}", args.join(" "));
    let text = capture(out, &what)?;
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "{what} printed nothing");
    Ok(text.to_string())
}

/// One GET, 5 s budget: does the service answer 200 right now?
async fn answers_200(url: &str) -> bool {
    let Ok(client) = hs_common::http::http_client(std::time::Duration::from_secs(5)) else {
        return false;
    };
    matches!(client.get(url).send().await, Ok(r) if r.status().is_success())
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
    let files = crate::restart_cmd::managed_compose_files(&hidden, scribe_cfg.local_server);
    // The same URLs the compose phase used to decide whether to recreate.
    // The distill compose file runs Qdrant only; the distill server is
    // always a native unit, checked below.
    for (name, label) in [("scribe", "scribe"), ("distill", "Qdrant")] {
        if !files.iter().any(|(n, _)| *n == name) {
            continue;
        }
        if let Some(url) = compose_health_url(name, &scribe_cfg)? {
            hs_common::compose::wait_for_url(&url, HEALTH_WAIT_SECS, label).await?;
            reporter.status("Health", &format!("{label}: OK"));
        }
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
        let err = upgrade_compose_service(&compose, None, &cf, "scribe", None, &reporter())
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
                upgrade_compose_service(&compose, None, &cf, "distill", None, &reporter())
                    .await
                    .is_err()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let compose = fake_compose(dir.path(), "none");
        let cf = dir.path().join("c.yml");
        std::fs::write(&cf, "").unwrap();
        upgrade_compose_service(&compose, None, &cf, "distill", None, &reporter())
            .await
            .unwrap();
        let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let verbs: Vec<&str> = log.lines().map(|l| l.split(' ').nth(2).unwrap()).collect();
        assert_eq!(verbs, ["pull", "down", "up"]);
    }

    // ── leave unchanged containers running ─────────────────────────

    fn images(services: usize, named: &[&str], running: &[&str]) -> ServiceImages {
        ServiceImages {
            services,
            named: named.iter().map(|s| s.to_string()).collect(),
            running: running.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn containers_are_unchanged_only_when_they_run_exactly_the_named_images() {
        assert!(images(1, &["8a80"], &["8a80"]).unchanged());
        // Docker prefixes ids with `sha256:`; podman does not.
        assert!(images(1, &["sha256:8a80"], &["8a80\n"]).unchanged());
        // The pull moved the tag: the container runs the old image.
        assert!(!images(1, &["9b91"], &["8a80"]).unchanged());
        // The container is not running.
        assert!(!images(1, &["8a80"], &[]).unchanged());
        // Two services, one container.
        assert!(!images(2, &["8a80", "c0de"], &["8a80"]).unchanged());
        // No services at all says nothing about the containers.
        assert!(!images(0, &[], &[]).unchanged());
    }

    #[test]
    fn service_images_come_from_the_resolved_config_and_require_an_image() {
        // `podman-compose config` output for the distill stack.
        let config = "services:\n  qdrant:\n    image: docker.io/qdrant/qdrant:latest\n    \
                      ports:\n    - 6333:6333\n    restart: always\n";
        assert_eq!(
            service_images(config).unwrap(),
            ["docker.io/qdrant/qdrant:latest"]
        );
        let built = "services:\n  app:\n    build: .\n";
        assert!(format!("{:#}", service_images(built).unwrap_err()).contains("names no image"));
    }

    /// Stand-ins for a compose runtime and its engine that describe one
    /// running Qdrant container; `tag_id` is what the tag names after the
    /// pull, the container always runs `8a80`.
    fn fake_runtime(dir: &Path, tag_id: &str) -> (ComposeCmd, String) {
        let log = dir.join("calls.log");
        let compose = dir.join("fake-compose");
        std::fs::write(
            &compose,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$3\" in\n\
                 config) printf 'services:\\n  qdrant:\\n    image: q:latest\\n' ;;\n\
                 ps) echo cid1 ;;\nesac\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        let engine = dir.join("fake-engine");
        std::fs::write(
            &engine,
            format!("#!/bin/sh\nif [ \"$1\" = image ]; then echo {tag_id}; else echo 8a80; fi\n"),
        )
        .unwrap();
        for f in [&compose, &engine] {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (
            ComposeCmd {
                bin: compose.to_string_lossy().into_owned(),
                args_prefix: vec![],
            },
            engine.to_string_lossy().into_owned(),
        )
    }

    fn verbs(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls.log"))
            .unwrap()
            .lines()
            .map(|l| l.split(' ').nth(2).unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn an_unchanged_healthy_service_is_left_running() {
        let healthy = serve(HashMap::from([("/healthz".to_string(), Route::ok("ok"))])).await;
        let url = format!("{healthy}/healthz");
        let dir = tempfile::tempdir().unwrap();
        let (compose, engine) = fake_runtime(dir.path(), "8a80");
        let cf = dir.path().join("c.yml");
        upgrade_compose_service(
            &compose,
            Some(&engine),
            &cf,
            "distill",
            Some(&url),
            &reporter(),
        )
        .await
        .unwrap();
        // Pulled and inspected; never stopped.
        assert_eq!(verbs(dir.path()), ["pull", "config", "ps"]);
    }

    #[tokio::test]
    async fn a_moved_tag_or_an_unhealthy_service_is_recreated() {
        let healthy = serve(HashMap::from([("/healthz".to_string(), Route::ok("ok"))])).await;
        let healthy = format!("{healthy}/healthz");
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/healthz", l.local_addr().unwrap())
        };
        for (tag_id, url) in [("9b91", healthy.as_str()), ("8a80", dead.as_str())] {
            let dir = tempfile::tempdir().unwrap();
            let (compose, engine) = fake_runtime(dir.path(), tag_id);
            let cf = dir.path().join("c.yml");
            upgrade_compose_service(
                &compose,
                Some(&engine),
                &cf,
                "distill",
                Some(url),
                &reporter(),
            )
            .await
            .unwrap();
            assert_eq!(
                verbs(dir.path()),
                ["pull", "config", "ps", "down", "up"],
                "tag {tag_id}, health {url}"
            );
        }
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

    /// `0.0.1-rc.358-47-gd4a0d79` is valid semver that outranks `rc.360`, so
    /// an unstripped development build never saw a newer release.
    #[test]
    fn a_development_build_is_compared_by_the_release_it_is_based_on() {
        let rc = |n: u64| semver::Version::parse(&format!("0.0.1-rc.{n}")).unwrap();
        let dev = describe_base_version("0.0.1-rc.358-47-gd4a0d79");
        assert_eq!(dev, rc(358));
        assert!(dev < rc(360));

        assert_eq!(describe_base_version("0.0.1-rc.360"), rc(360));
        assert_eq!(
            describe_base_version("0.0.1"),
            semver::Version::new(0, 0, 1)
        );
        // No tag in the checkout: a bare hash is based on no release.
        assert_eq!(
            describe_base_version("d4a0d79"),
            semver::Version::new(0, 0, 0)
        );
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
