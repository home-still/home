//! Client-side gateway registry queries.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::auth::client::AuthenticatedClient;

/// Maximum time to wait for gateway registry discovery. Discovery
/// failures (incl. timeouts) propagate as errors — there is no fallback
/// to a default or config-defined server pool (ONE PATH).
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

    let http = crate::http::http_client(DISCOVERY_TIMEOUT)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client::CloudCredentials;

    #[tokio::test]
    async fn discover_servers_propagates_gateway_unreachable_error() {
        // Point at a port nothing is listening on. `discover_servers` must
        // return `Err` — never silently swallow into an empty Vec or a
        // hardcoded default. This is the regression guard for the deleted
        // `discover_or_fallback` helper.
        let creds = CloudCredentials {
            gateway_url: "http://127.0.0.1:1".to_string(),
            device_name: "test-device".to_string(),
            // Well-formed enough to build a client; the refresh will fail
            // against port 1, which is what the test is checking.
            refresh_token: "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0In0.sig".to_string(),
            cf_access_client_id: None,
            cf_access_client_secret: None,
        };
        let auth = AuthenticatedClient::new(creds).expect("build client");
        let result = discover_servers(&auth, "scribe").await;
        assert!(
            result.is_err(),
            "gateway at port 1 should fail, got {result:?}"
        );
    }
}
