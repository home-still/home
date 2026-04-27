//! Authenticated HTTP client for cloud-connected services.
//!
//! Handles token storage, automatic refresh, and transparent auth header injection.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::token::TokenClaims;

/// Stored credentials for a cloud-enrolled device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudCredentials {
    /// Gateway URL (e.g., "https://cloud.lolzlab.com")
    pub gateway_url: String,
    /// Long-lived refresh token (7-day TTL)
    pub refresh_token: String,
    /// Device name used during enrollment
    pub device_name: String,
    /// Cloudflare Access service token client ID (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_access_client_id: Option<String>,
    /// Cloudflare Access service token client secret (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_access_client_secret: Option<String>,
}

impl CloudCredentials {
    /// Default path for credential storage.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(crate::HIDDEN_DIR)
            .join("cloud-token")
    }

    /// Load credentials from disk.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    /// Save credentials to disk with restricted permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, &data)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// An HTTP client that automatically attaches bearer tokens and refreshes them.
pub struct AuthenticatedClient {
    http: reqwest::Client,
    credentials: CloudCredentials,
    /// Cached access token (short-lived, refreshed automatically)
    access_token: Mutex<Option<String>>,
}

impl AuthenticatedClient {
    /// Create a new authenticated client from stored credentials.
    pub fn new(credentials: CloudCredentials) -> anyhow::Result<Self> {
        Ok(Self {
            http: crate::http::http_client(std::time::Duration::from_secs(10))?,
            credentials,
            access_token: Mutex::new(None),
        })
    }

    /// Load credentials from the default path and create a client.
    pub fn from_default_path() -> anyhow::Result<Self> {
        let creds = CloudCredentials::load(&CloudCredentials::default_path())?;
        Self::new(creds)
    }

    /// Get the gateway URL.
    pub fn gateway_url(&self) -> &str {
        &self.credentials.gateway_url
    }

    /// Get a valid access token, refreshing if needed.
    pub async fn get_access_token(&self) -> anyhow::Result<String> {
        // Check cached token
        {
            let guard = self.access_token.lock().unwrap();
            if let Some(ref token) = *guard {
                // Parse just enough to check expiry (don't validate signature — we're the client)
                if let Some((payload_b64, _)) = token.split_once('.') {
                    if let Ok(payload_bytes) = base64::Engine::decode(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                        payload_b64,
                    ) {
                        if let Ok(claims) = serde_json::from_slice::<TokenClaims>(&payload_bytes) {
                            if claims.ttl_secs() > 60 {
                                return Ok(token.clone());
                            }
                        }
                    }
                }
            }
        }

        // Refresh the token
        let new_token = self.refresh_access_token().await?;
        let mut guard = self.access_token.lock().unwrap();
        *guard = Some(new_token.clone());
        Ok(new_token)
    }

    /// Request a new access token using the refresh token.
    async fn refresh_access_token(&self) -> anyhow::Result<String> {
        let url = format!("{}/cloud/refresh", self.credentials.gateway_url);

        let mut req = self
            .http
            .post(&url)
            .bearer_auth(&self.credentials.refresh_token);

        // Add Cloudflare Access headers if available
        if let (Some(ref id), Some(ref secret)) = (
            &self.credentials.cf_access_client_id,
            &self.credentials.cf_access_client_secret,
        ) {
            req = req
                .header("CF-Access-Client-Id", id)
                .header("CF-Access-Client-Secret", secret);
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Token refresh failed ({status}): {body}");
        }

        #[derive(Deserialize)]
        struct RefreshResponse {
            access_token: String,
        }

        let body: RefreshResponse = resp.json().await?;
        Ok(body.access_token)
    }

    /// Build a reqwest::Client with the current access token as default bearer auth.
    ///
    /// This is useful for passing to `ScribeClient::new_with_client()` etc.
    pub async fn build_reqwest_client(&self) -> anyhow::Result<reqwest::Client> {
        let token = self.get_access_token().await?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );

        // Add Cloudflare Access headers if available
        if let (Some(ref id), Some(ref secret)) = (
            &self.credentials.cf_access_client_id,
            &self.credentials.cf_access_client_secret,
        ) {
            headers.insert("CF-Access-Client-Id", id.parse().unwrap());
            headers.insert("CF-Access-Client-Secret", secret.parse().unwrap());
        }

        Ok(reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .default_headers(headers)
            .build()?)
    }
}

/// Check if a server URL appears to be a cloud gateway URL.
pub fn is_cloud_url(server_url: &str) -> bool {
    server_url.starts_with("https://")
}

/// Build an authenticated reqwest client for a cloud URL, or a plain one for local URLs.
///
/// Returns `None` if the URL is local (no auth needed), or `Some(client)` for cloud URLs.
pub async fn maybe_authenticated_client(
    server_url: &str,
) -> anyhow::Result<Option<reqwest::Client>> {
    if !is_cloud_url(server_url) {
        return Ok(None);
    }

    let auth_client = AuthenticatedClient::from_default_path()?;
    let client = auth_client.build_reqwest_client().await?;
    Ok(Some(client))
}
