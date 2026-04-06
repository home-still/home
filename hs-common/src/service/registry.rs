//! Client-side gateway registry queries.

use std::time::Duration;

use serde::Deserialize;

use crate::auth::client::AuthenticatedClient;

/// Maximum time to wait for gateway registry discovery before falling back.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
struct ServicesResponse {
    services: Vec<ServiceInfo>,
}

#[derive(Deserialize)]
struct ServiceInfo {
    service_type: String,
    url: String,
    enabled: bool,
    healthy: bool,
}

/// Query the gateway registry for healthy, enabled servers of a given type.
/// Returns a list of server URLs, or an error if the gateway is unreachable.
pub async fn discover_servers(
    auth: &AuthenticatedClient,
    service_type: &str,
) -> anyhow::Result<Vec<String>> {
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

    let urls: Vec<String> = resp
        .services
        .into_iter()
        .filter(|s| s.service_type == service_type && s.enabled && s.healthy)
        .map(|s| s.url)
        .collect();

    Ok(urls)
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
