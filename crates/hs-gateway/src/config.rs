//! Gateway configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;

use crate::auth::SERVICES;
use crate::backend_url;

/// Gateway configuration loaded from the cloud.gateway section of config.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// Address to listen on, e.g. `0.0.0.0:7440`
    pub listen: String,

    /// Path to the HMAC secret key file. The admin key and the revocation list
    /// live beside it (`cloud-admin.key`, `cloud-revoked.json`).
    #[serde(default = "hs_common::auth::token::default_secret_path")]
    pub secret_path: PathBuf,

    /// Previous signing secret, set only during a key-rotation grace period:
    /// tokens signed with it are still accepted, new tokens are always signed
    /// with the current secret. Must exist when set.
    #[serde(default)]
    pub previous_secret_path: Option<PathBuf>,

    /// Access token TTL in seconds (default: 14400 = 4 hours)
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,

    /// Refresh token TTL in seconds (default: 604800 = 7 days)
    #[serde(default = "default_refresh_ttl")]
    pub refresh_ttl_secs: u64,

    /// Service routing: service name (`scribe`, `distill`, `mcp`) -> backend URL
    /// e.g., { "scribe": "http://scribe.example.local:7433" }
    pub routes: HashMap<String, String>,

    /// Proxied requests allowed in flight at once; excess requests get 503.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_proxy_requests: usize,

    /// Largest request body the proxy will stream to a backend.
    #[serde(default = "default_max_body")]
    pub max_request_body_bytes: u64,

    /// Backend connect timeout.
    #[serde(default = "default_connect_timeout")]
    pub backend_connect_timeout_secs: u64,

    /// Backend stall timeout: longest wait for the response headers or for the
    /// next chunk of the response body.
    #[serde(default = "default_read_timeout")]
    pub backend_read_timeout_secs: u64,

    /// Longest a single proxied request may take end to end.
    #[serde(default = "default_total_timeout")]
    pub backend_total_timeout_secs: u64,

    /// POSTs per minute allowed on each of `/cloud/enroll`, `/authorize`,
    /// `/token` and `/register`.
    #[serde(default = "default_auth_rate")]
    pub auth_rate_limit_per_minute: u32,
}

fn default_token_ttl() -> u64 {
    14400 // 4 hours
}

fn default_refresh_ttl() -> u64 {
    604800
}

fn default_max_concurrent() -> usize {
    64
}

fn default_max_body() -> u64 {
    256 * 1024 * 1024
}

fn default_connect_timeout() -> u64 {
    10
}

fn default_read_timeout() -> u64 {
    600
}

fn default_total_timeout() -> u64 {
    3600
}

fn default_auth_rate() -> u32 {
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

        let mut config: Self = serde_json::from_value(section.clone()).with_context(|| {
            format!("{}: invalid `cloud.gateway` section", config_path.display())
        })?;

        config.validate().with_context(|| {
            format!("{}: invalid `cloud.gateway` section", config_path.display())
        })?;
        Ok(config)
    }

    fn validate(&mut self) -> anyhow::Result<()> {
        if self.routes.is_empty() {
            bail!("`cloud.gateway.routes` is empty — the gateway would route nothing");
        }
        let mut routes = HashMap::new();
        for (service, url) in &self.routes {
            if !SERVICES.contains(&service.as_str()) {
                bail!(
                    "`cloud.gateway.routes.{service}` is not a routable service \
                     (must be one of: {})",
                    SERVICES.join(", ")
                );
            }
            let base = backend_url::normalize_route(url)
                .map_err(|e| anyhow!("`cloud.gateway.routes.{service}` ({url}): {e}"))?;
            routes.insert(service.clone(), base);
        }
        self.routes = routes;

        for (name, value) in [
            ("token_ttl_secs", self.token_ttl_secs),
            ("refresh_ttl_secs", self.refresh_ttl_secs),
            (
                "backend_connect_timeout_secs",
                self.backend_connect_timeout_secs,
            ),
            ("backend_read_timeout_secs", self.backend_read_timeout_secs),
            (
                "backend_total_timeout_secs",
                self.backend_total_timeout_secs,
            ),
            ("max_request_body_bytes", self.max_request_body_bytes),
            (
                "max_concurrent_proxy_requests",
                self.max_concurrent_proxy_requests as u64,
            ),
            (
                "auth_rate_limit_per_minute",
                u64::from(self.auth_rate_limit_per_minute),
            ),
        ] {
            if value == 0 {
                bail!("`cloud.gateway.{name}` must be greater than zero");
            }
        }
        Ok(())
    }

    /// Where `hs cloud revoke` records revoked devices.
    pub fn revocation_path(&self) -> PathBuf {
        self.secret_path.with_file_name("cloud-revoked.json")
    }

    /// Where the admin key lives.
    pub fn admin_key_path(&self) -> PathBuf {
        hs_common::auth::token::admin_key_path_for(&self.secret_path)
    }

    /// Resolve a service name to its backend URL.
    pub fn backend_for(&self, service: &str) -> Option<&str> {
        self.routes.get(service).map(|s| s.as_str())
    }
}

/// Validate the externally visible gateway URL: it is handed to OAuth clients
/// as the issuer and authorization endpoint, so it must be an explicit https
/// origin. Returns it without a trailing slash.
pub fn validate_gateway_url(raw: Option<&str>) -> anyhow::Result<String> {
    let raw = raw.ok_or_else(|| {
        anyhow!(
            "the gateway URL is required: pass --gateway-url https://<your-public-hostname> \
             (it is published in OAuth metadata and must be the public https origin)"
        )
    })?;
    let url =
        url::Url::parse(raw).with_context(|| format!("gateway URL {raw:?} is not a valid URL"))?;
    if url.scheme() != "https" {
        bail!("gateway URL {raw:?} must use https");
    }
    if url.host_str().is_none() {
        bail!("gateway URL {raw:?} has no host");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.path() != "/" && !url.path().is_empty())
    {
        bail!("gateway URL {raw:?} must be a bare origin (scheme://host[:port])");
    }
    Ok(backend_url::canonical(&url))
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
        assert_eq!(
            config.admin_key_path().file_name().unwrap(),
            "cloud-admin.key"
        );
        assert_eq!(
            config.revocation_path().parent(),
            config.secret_path.parent()
        );
    }

    #[test]
    fn routes_are_normalized() {
        let config = GatewayConfig::from_yaml(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    routes:\n      scribe: http://big.example.local:7433/\n",
            Path::new(PATH),
        )
        .unwrap();
        assert_eq!(
            config.backend_for("scribe"),
            Some("http://big.example.local:7433")
        );
    }

    #[test]
    fn a_route_that_can_never_be_reached_is_an_error() {
        let msg = err_of(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    routes:\n      scribe: http://127.0.0.1:7433\n      searhc: http://127.0.0.1:7434\n",
        );
        assert!(msg.contains("searhc"), "{msg}");
    }

    #[test]
    fn a_malformed_route_url_is_an_error() {
        let msg = err_of(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    routes:\n      scribe: ftp://host\n",
        );
        assert!(msg.contains("scribe"), "{msg}");
    }

    #[test]
    fn zero_limits_are_errors() {
        let msg = err_of(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    max_concurrent_proxy_requests: 0\n    routes:\n      mcp: http://127.0.0.1:7445\n",
        );
        assert!(msg.contains("max_concurrent_proxy_requests"), "{msg}");
    }

    #[test]
    fn gateway_url_must_be_an_explicit_https_origin() {
        assert_eq!(
            validate_gateway_url(Some("https://cloud.example.com/"))
                .as_deref()
                .ok(),
            Some("https://cloud.example.com")
        );
        assert!(validate_gateway_url(None).is_err());
        for bad in [
            "http://cloud.example.com",
            "http://127.0.0.1:7440",
            "cloud.example.com",
            "https://cloud.example.com/mcp",
            "https://user:pw@cloud.example.com",
            "https://cloud.example.com?x=1",
            "",
        ] {
            assert!(validate_gateway_url(Some(bad)).is_err(), "{bad:?}");
        }
    }
}
