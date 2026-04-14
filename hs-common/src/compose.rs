//! Container-compose runtime detection and execution.

use anyhow::Result;

/// Detected compose command: "docker compose", "docker-compose", or "podman-compose".
pub struct ComposeCmd {
    pub bin: String,
    pub args_prefix: Vec<String>,
}

impl ComposeCmd {
    /// Auto-detect the best available compose runtime.
    pub async fn detect() -> Option<Self> {
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

    /// Run a compose command with visible output.
    pub async fn run(&self, args: &[&str]) -> Result<std::process::ExitStatus> {
        let mut full_args: Vec<&str> = self.args_prefix.iter().map(|s| s.as_str()).collect();
        full_args.extend_from_slice(args);
        let status = tokio::process::Command::new(&self.bin)
            .args(&full_args)
            .status()
            .await?;
        Ok(status)
    }

    /// Run a compose command silently, returning success/failure.
    pub async fn run_silent(&self, args: &[&str]) -> bool {
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
    pub async fn run_capture(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut full_args: Vec<&str> = self.args_prefix.iter().map(|s| s.as_str()).collect();
        full_args.extend_from_slice(args);
        let output = tokio::process::Command::new(&self.bin)
            .args(&full_args)
            .output()
            .await?;
        Ok(output)
    }

    /// Run "exec <service> <cmd...>" via compose.
    pub async fn exec_run(
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

/// Filter compose stderr, returning only actionable error lines.
/// Strips podman banners, self-resolving pod conflicts, Docker socket warnings,
/// and other noise that isn't useful to the user.
pub fn filter_compose_stderr(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| {
            let l = line.trim();
            !l.is_empty()
                && !l.contains("Emulate Docker CLI using podman")
                && !l.contains("nodocker to quiet msg")
                && !l.contains("Executing external compose provider")
                && !l.contains("podman-compose(1)")
                && !l.contains("cannot remove container")
                && !l.contains("container state improper")
                && !l.contains("has associated containers")
                && !l.contains("Use -f to forcibly")
                && !l.contains("no container with name or ID")
                && !l.contains("no container with ID or name")
                && !l.contains("Cannot connect to the Docker daemon")
                && !l.contains("Is the docker daemon running")
                && !l.starts_with("WARN[")
        })
        .collect()
}

/// Check if a command exists and runs successfully.
pub async fn check_command(cmd: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll a URL until it returns 200 OK, or bail after `timeout_secs`.
pub async fn wait_for_url(url: &str, timeout_secs: u64, label: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for {} at {} ({}s)",
                label,
                url,
                timeout_secs
            );
        }
        if client
            .get(url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}
