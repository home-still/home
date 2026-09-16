//! Gateway configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;

/// Gateway configuration loaded from the cloud.gateway section of config.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// Address to listen on, e.g. `0.0.0.0:7440`
    pub listen: String,

    /// Path to the HMAC secret key file
    #[serde(default = "default_secret_path")]
    pub secret_path: PathBuf,

    /// Access token TTL in seconds (default: 14400 = 4 hours)
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,

    /// Refresh token TTL in seconds (default: 604800 = 7 days)
    #[serde(default = "default_refresh_ttl")]
    pub refresh_ttl_secs: u64,

    /// Key rotation interval in days (default: 30)
    #[serde(default = "default_rotation_days")]
    pub key_rotation_days: u64,

    /// Service routing: path prefix -> backend URL
    /// e.g., { "scribe": "http://scribe.example.local:7433" }
    pub routes: HashMap<String, String>,
}

fn default_secret_path() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(hs_common::HIDDEN_DIR).join("cloud-secret.key"),
        // No home dir means there is no config file to read either; keep the
        // path relative rather than inventing an absolute one.
        None => PathBuf::from(hs_common::HIDDEN_DIR).join("cloud-secret.key"),
    }
}

fn default_token_ttl() -> u64 {
    14400 // 4 hours
}

fn default_refresh_ttl() -> u64 {
    604800
}

fn default_rotation_days() -> u64 {
    30
}

impl GatewayConfig {
    /// Load from the cloud.gateway section of ~/.home-still/config.yaml.
    ///
    /// There is no default gateway: a missing or unreadable section would
    /// otherwise bind a made-up address and route nothing while `/health`
    /// still answered `ok`.
    pub fn load() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| {
            anyhow!(
                "cannot determine home directory for {}",
                hs_common::CONFIG_REL_PATH
            )
        })?;
        let config_path = home.join(hs_common::CONFIG_REL_PATH);

        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        Self::from_yaml(&contents, &config_path)
    }

    /// Parse the `cloud.gateway` section out of `contents`. Split from
    /// [`Self::load`] so the section's rules can be tested without `$HOME`.
    pub fn from_yaml(contents: &str, config_path: &Path) -> anyhow::Result<Self> {
        let root: serde_json::Value = serde_yaml_ng::from_str(contents)
            .with_context(|| format!("{}: not valid YAML", config_path.display()))?;

        let section = root
            .get("cloud")
            .and_then(|cloud| cloud.get("gateway"))
            .ok_or_else(|| anyhow!("{}: missing `cloud.gateway` section", config_path.display()))?;

        let config: Self = serde_json::from_value(section.clone()).with_context(|| {
            format!("{}: invalid `cloud.gateway` section", config_path.display())
        })?;

        if config.routes.is_empty() {
            bail!(
                "{}: `cloud.gateway.routes` is empty — the gateway would route nothing",
                config_path.display()
            );
        }

        Ok(config)
    }

    /// Load the HMAC secret from disk, or generate + save if missing.
    pub fn load_or_create_secret(&self) -> anyhow::Result<Vec<u8>> {
        if self.secret_path.exists() {
            let data = std::fs::read(&self.secret_path)?;
            if data.len() >= 32 {
                return Ok(data);
            }
        }

        let secret = hs_common::auth::token::generate_secret();
        if let Some(parent) = self.secret_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.secret_path, &secret)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.secret_path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(secret)
    }

    /// Resolve a service name to its backend URL.
    pub fn backend_for(&self, service: &str) -> Option<&str> {
        self.routes.get(service).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/tmp/config.yaml";

    fn err_of(yaml: &str) -> String {
        let e = GatewayConfig::from_yaml(yaml, Path::new(PATH)).expect_err("should be rejected");
        format!("{e:#}")
    }

    #[test]
    fn missing_gateway_section_is_an_error() {
        let msg = err_of("home:\n  project_dir: /home-still\n");
        assert!(msg.contains("cloud.gateway"), "{msg}");
    }

    #[test]
    fn empty_routes_is_an_error() {
        let msg = err_of("cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    routes: {}\n");
        assert!(msg.contains("routes"), "{msg}");
    }

    #[test]
    fn missing_listen_is_an_error() {
        let msg = err_of("cloud:\n  gateway:\n    routes:\n      mcp: http://127.0.0.1:7445\n");
        assert!(msg.contains("listen"), "{msg}");
    }

    #[test]
    fn valid_section_parses() {
        let config = GatewayConfig::from_yaml(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    routes:\n      mcp: http://127.0.0.1:7445\n      scribe: http://127.0.0.1:7435\n      distill: http://127.0.0.1:7434\n",
            Path::new(PATH),
        )
        .expect("valid section");

        assert_eq!(config.listen, "127.0.0.1:7440");
        assert_eq!(config.backend_for("mcp"), Some("http://127.0.0.1:7445"));
        assert_eq!(config.routes.len(), 3);
        assert_eq!(config.token_ttl_secs, 14400);
        assert_eq!(
            config.secret_path,
            dirs::home_dir()
                .expect("test host has a home dir")
                .join(hs_common::HIDDEN_DIR)
                .join("cloud-secret.key")
        );
    }
}
