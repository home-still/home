//! Client-side gateway registry queries.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::auth::client::{AuthenticatedClient, CloudCredentials, RefreshRejected};

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

/// Why a registry query produced no answer. Each variant needs a different
/// operator action, which is why none of them collapses into "no servers".
#[derive(Debug)]
pub enum DiscoveryError {
    /// No cloud credentials on this host: it never ran `hs cloud enroll`.
    NotEnrolled,
    /// The credentials file exists but cannot be used (corrupt, unparsable).
    CredentialsUnreadable(String),
    /// The gateway refused the refresh token or the access token (HTTP
    /// 401/403): re-enroll the device.
    CredentialsRejected { status: u16 },
    /// The gateway could not be reached (connect failure or timeout).
    GatewayUnreachable(String),
    /// The gateway answered with an error status or an unparsable body.
    Failed(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnrolled => {
                f.write_str("not enrolled: no cloud credentials (run `hs cloud enroll`)")
            }
            Self::CredentialsUnreadable(why) => write!(f, "cloud credentials unusable: {why}"),
            Self::CredentialsRejected { status } => write!(
                f,
                "gateway rejected this device's credentials (HTTP {status}); re-enroll with `hs cloud enroll`"
            ),
            Self::GatewayUnreachable(why) => write!(f, "gateway unreachable: {why}"),
            Self::Failed(why) => write!(f, "gateway registry query failed: {why}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

fn classify_http(e: &reqwest::Error) -> DiscoveryError {
    if e.is_connect() || e.is_timeout() {
        return DiscoveryError::GatewayUnreachable(e.to_string());
    }
    match e.status().map(|s| s.as_u16()) {
        Some(status @ (401 | 403)) => DiscoveryError::CredentialsRejected { status },
        _ => DiscoveryError::Failed(e.to_string()),
    }
}

fn classify_token_error(e: anyhow::Error) -> DiscoveryError {
    if let Some(r) = e.downcast_ref::<RefreshRejected>() {
        return DiscoveryError::CredentialsRejected { status: r.status };
    }
    match e.downcast_ref::<reqwest::Error>() {
        Some(re) => classify_http(re),
        None => DiscoveryError::Failed(format!("{e:#}")),
    }
}

async fn fetch_all(auth: &AuthenticatedClient) -> Result<Vec<ServiceInfo>, DiscoveryError> {
    let query = async {
        let token = auth
            .get_access_token()
            .await
            .map_err(classify_token_error)?;
        let gateway_url = auth.gateway_url();
        let http = crate::http::http_client(DISCOVERY_TIMEOUT)
            .map_err(|e| DiscoveryError::Failed(format!("{e:#}")))?;
        let resp: ServicesResponse = http
            .get(format!("{gateway_url}/registry/services"))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| classify_http(&e))?
            .error_for_status()
            .map_err(|e| classify_http(&e))?
            .json()
            .await
            .map_err(|e| classify_http(&e))?;
        Ok(resp.services)
    };
    match tokio::time::timeout(DISCOVERY_TIMEOUT, query).await {
        Ok(out) => out,
        Err(_) => Err(DiscoveryError::GatewayUnreachable(format!(
            "no answer within {}s",
            DISCOVERY_TIMEOUT.as_secs()
        ))),
    }
}

/// Query the gateway registry for healthy, enabled servers of a given type.
/// Returns a list of server URLs, or an error if the gateway is unreachable.
/// The error is a [`DiscoveryError`] (downcast it to tell a rejected token
/// from an unreachable gateway).
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
///
/// An empty `Ok` means the gateway answered and has no such instance. Every
/// reason it could not answer — this host is not enrolled, the refresh token
/// was rejected, the gateway is down — is a distinct [`DiscoveryError`].
pub async fn discover_instances(service_type: &str) -> Result<Vec<ServiceInfo>, DiscoveryError> {
    discover_instances_with(&CloudCredentials::default_path(), service_type).await
}

async fn discover_instances_with(
    credentials_path: &std::path::Path,
    service_type: &str,
) -> Result<Vec<ServiceInfo>, DiscoveryError> {
    let creds = CloudCredentials::load(credentials_path).map_err(|e| {
        let missing = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
        if missing {
            DiscoveryError::NotEnrolled
        } else {
            DiscoveryError::CredentialsUnreadable(format!("{e:#}"))
        }
    })?;
    let auth = AuthenticatedClient::new(creds)
        .map_err(|e| DiscoveryError::CredentialsUnreadable(format!("{e:#}")))?;
    Ok(fetch_all(&auth)
        .await?
        .into_iter()
        .filter(|s| s.service_type == service_type)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client::CloudCredentials;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn creds_for(gateway_url: &str) -> CloudCredentials {
        CloudCredentials {
            gateway_url: gateway_url.to_string(),
            device_name: "test-device".to_string(),
            // Well-formed enough to build a client; what happens next is
            // up to the (fake) gateway.
            refresh_token: "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0In0.sig".to_string(),
            cf_access_client_id: None,
            cf_access_client_secret: None,
        }
    }

    /// Serve canned HTTP responses on a loopback port, routed by request
    /// line prefix. Returns the base URL.
    async fn fake_gateway(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = head.lines().next().unwrap_or("").to_string();
                    let (status, body) = routes
                        .iter()
                        .find(|(prefix, _, _)| line.contains(prefix))
                        .map(|(_, s, b)| (*s, *b))
                        .unwrap_or((404, "{}"));
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn write_creds(dir: &std::path::Path, gateway_url: &str) -> std::path::PathBuf {
        let path = dir.join("cloud-token");
        creds_for(gateway_url).save(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn discover_servers_propagates_gateway_unreachable_error() {
        // Point at a port nothing is listening on. `discover_servers` must
        // return `Err` — never silently swallow into an empty Vec or a
        // hardcoded default. This is the regression guard for the deleted
        // `discover_or_fallback` helper.
        let auth = AuthenticatedClient::new(creds_for("http://127.0.0.1:1")).expect("build client");
        let result = discover_servers(&auth, "scribe").await;
        assert!(
            result.is_err(),
            "gateway at port 1 should fail, got {result:?}"
        );
    }

    /// RA-76: each failure mode surfaces as its own error — none of them is
    /// an empty instance list.
    #[tokio::test]
    async fn no_credentials_is_not_enrolled_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let err = discover_instances_with(&dir.path().join("absent"), "scribe")
            .await
            .unwrap_err();
        assert!(matches!(err, DiscoveryError::NotEnrolled), "{err:?}");
    }

    #[tokio::test]
    async fn corrupt_credentials_are_unreadable_not_not_enrolled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-token");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = discover_instances_with(&path, "scribe").await.unwrap_err();
        assert!(
            matches!(err, DiscoveryError::CredentialsUnreadable(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unreachable_gateway_is_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(dir.path(), "http://127.0.0.1:1");
        let err = discover_instances_with(&path, "scribe").await.unwrap_err();
        assert!(
            matches!(err, DiscoveryError::GatewayUnreachable(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejected_refresh_token_is_its_own_error() {
        let gateway = fake_gateway(vec![("/cloud/refresh", 401, r#"{"error":"expired"}"#)]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(dir.path(), &gateway);
        let err = discover_instances_with(&path, "scribe").await.unwrap_err();
        assert!(
            matches!(err, DiscoveryError::CredentialsRejected { status: 401 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn gateway_error_status_is_a_failure_not_an_empty_list() {
        let gateway = fake_gateway(vec![
            ("/cloud/refresh", 200, r#"{"access_token":"a.b"}"#),
            ("/registry/services", 500, "{}"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(dir.path(), &gateway);
        let err = discover_instances_with(&path, "scribe").await.unwrap_err();
        assert!(matches!(err, DiscoveryError::Failed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn healthy_gateway_lists_only_the_requested_service_type() {
        let services = r#"{"services":[
            {"service_type":"scribe","url":"http://a","enabled":true,"healthy":false},
            {"service_type":"distill","url":"http://b","enabled":true,"healthy":true}
        ]}"#;
        let gateway = fake_gateway(vec![
            ("/cloud/refresh", 200, r#"{"access_token":"a.b"}"#),
            ("/registry/services", 200, services),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(dir.path(), &gateway);
        let out = discover_instances_with(&path, "scribe").await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "http://a");
        assert!(!out[0].healthy, "unhealthy instances are still listed");
    }
}
