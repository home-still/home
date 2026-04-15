//! Gateway configuration.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Gateway configuration loaded from the cloud.gateway section of config.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// Address to listen on (default: 127.0.0.1:7440)
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Path to the HMAC secret key file
    #[serde(default = "default_secret_path")]
    pub secret_path: PathBuf,

    /// Access token TTL in seconds (default: 3600 = 1 hour)
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,

    /// Refresh token TTL in seconds (default: 604800 = 7 days)
    #[serde(default = "default_refresh_ttl")]
    pub refresh_ttl_secs: u64,

    /// Key rotation interval in days (default: 30)
    #[serde(default = "default_rotation_days")]
    pub key_rotation_days: u64,

    /// Service routing: path prefix -> backend URL
    /// e.g., { "scribe": "http://192.168.1.110:7433" }
    #[serde(default)]
    pub routes: HashMap<String, String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            secret_path: default_secret_path(),
            token_ttl_secs: default_token_ttl(),
            refresh_ttl_secs: default_refresh_ttl(),
            key_rotation_days: default_rotation_days(),
            routes: HashMap::new(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:7440".into()
}

fn default_secret_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
        .join("cloud-secret.key")
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
    pub fn load() -> anyhow::Result<Self> {
        let home = dirs::home_dir().unwrap_or_default();
        let config_path = home.join(hs_common::CONFIG_REL_PATH);

        let contents = std::fs::read_to_string(&config_path).unwrap_or_default();
        let root: serde_json::Value =
            serde_yaml_ng::from_str(&contents).unwrap_or(serde_json::Value::Null);

        let gateway_section = root.get("cloud").and_then(|c| c.get("gateway"));

        match gateway_section {
            Some(section) => {
                let config: Self = serde_json::from_value(section.clone()).unwrap_or_default();
                Ok(config)
            }
            None => Ok(Self::default()),
        }
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
