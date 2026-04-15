//! Client-side gateway registry queries.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::auth::client::AuthenticatedClient;

/// Maximum time to wait for gateway registry discovery before falling back.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
struct ServicesResponse {
    services: Vec<ServiceInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceInfo {
    pub service_type: String,
    pub url: String,
    #[serde(default)]
    pub device_name: String,
    pub enabled: bool,
    pub healthy: bool,
    #[serde(default)]
    pub metadata: ServiceMetadata,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ServiceMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

async fn fetch_all(auth: &AuthenticatedClient) -> anyhow::Result<Vec<ServiceInfo>> {
    let token = auth.get_access_token().await?;
    let gateway_url = auth.gateway_url();

    let http = reqwest::Client::builder()
        .connect_timeout(DISCOVERY_TIMEOUT)
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp: ServicesResponse = http
        .get(format!("{gateway_url}/registry/services"))
        .bearer_auth(&token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.services)
}

/// Query the gateway registry for healthy, enabled servers of a given type.
/// Returns a list of server URLs, or an error if the gateway is unreachable.
pub async fn discover_servers(
    auth: &AuthenticatedClient,
    service_type: &str,
) -> anyhow::Result<Vec<String>> {
    let urls = fetch_all(auth)
        .await?
        .into_iter()
        .filter(|s| s.service_type == service_type && s.enabled && s.healthy)
        .map(|s| s.url)
        .collect();
    Ok(urls)
}

/// Query the gateway registry for every registered instance of a service type
/// (regardless of health/enabled). Callers render this as the Services panel
/// and can show unhealthy or disabled rows distinctly.
pub async fn discover_instances(service_type: &str) -> Vec<ServiceInfo> {
    let Ok(auth) = AuthenticatedClient::from_default_path() else {
        return Vec::new();
    };
    let result = tokio::time::timeout(DISCOVERY_TIMEOUT, fetch_all(&auth))
        .await
        .unwrap_or(Err(anyhow::anyhow!("timeout")));
    match result {
        Ok(all) => all
            .into_iter()
            .filter(|s| s.service_type == service_type)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Try to discover servers from the gateway registry, falling back to the given defaults.
/// Times out after 3 seconds to avoid blocking CLI commands when the gateway is down.
pub async fn discover_or_fallback(
    service_type: &str,
    fallback_servers: Vec<String>,
) -> Vec<String> {
    match AuthenticatedClient::from_default_path() {
        Ok(auth) => {
            let result: Result<Vec<String>, _> =
                tokio::time::timeout(DISCOVERY_TIMEOUT, discover_servers(&auth, service_type))
                    .await
                    .unwrap_or(Err(anyhow::anyhow!("timeout")));

            match result {
                Ok(urls) if !urls.is_empty() => urls,
                _ => fallback_servers,
            }
        }
        Err(_) => fallback_servers,
    }
}
