//! Authenticated HTTP client for cloud-connected services.
//!
//! Handles token storage, automatic refresh, and transparent auth header injection.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::token::TokenClaims;

/// Stored credentials for a cloud-enrolled device.
///
/// `Debug` is hand-written so the refresh token and Cloudflare Access secret
/// never reach a log line through `{:?}`.
#[derive(Clone, Serialize, Deserialize)]
pub struct CloudCredentials {
    /// Gateway URL (e.g., "https://<gateway-domain>")
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

impl std::fmt::Debug for CloudCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudCredentials")
            .field("gateway_url", &self.gateway_url)
            .field("refresh_token", &"<redacted>")
            .field("device_name", &self.device_name)
            .field("cf_access_client_id", &self.cf_access_client_id)
            .field(
                "cf_access_client_secret",
                &self.cf_access_client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The gateway refused the stored refresh token (HTTP 401/403): it expired
/// or was revoked, and the device must be re-enrolled. Distinct from the
/// gateway being unreachable, which a retry can fix.
#[derive(Debug)]
pub struct RefreshRejected {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for RefreshRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gateway rejected the refresh token ({}): {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for RefreshRejected {}

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

    /// Save credentials to disk, readable only by the owner.
    ///
    /// Written to a sibling temp file created with mode 0600 and renamed into
    /// place, so the secret is never visible with default-umask permissions
    /// and a crash mid-write cannot leave a truncated credentials file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;

        let mut tmp_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("credentials path {path:?} has no file name"))?
            .to_os_string();
        tmp_name.push(format!(".{}.tmp", std::process::id()));
        let tmp = path.with_file_name(tmp_name);

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let written = (|| -> std::io::Result<()> {
            let mut f = opts.open(&tmp)?;
            f.write_all(data.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = written {
            // Best effort: the write error is the one worth reporting.
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
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
            if matches!(status.as_u16(), 401 | 403) {
                return Err(RefreshRejected {
                    status: status.as_u16(),
                    body,
                }
                .into());
            }
            anyhow::bail!("Token refresh failed ({status}): {body}");
        }

        #[derive(Deserialize)]
        struct RefreshResponse {
            access_token: String,
        }

        let body: RefreshResponse = resp.json().await?;
        Ok(body.access_token)
    }

    /// Attach a bearer token to `req` — fetched now, refreshed when within
    /// 60 s of expiry — plus the Cloudflare Access headers when configured.
    /// Called at send time for every request, so a long-lived process never
    /// carries a stale token. A rejected refresh token is an `Err`
    /// ([`RefreshRejected`]); nothing retries.
    pub async fn authorize(
        &self,
        req: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let token = self.get_access_token().await?;
        let mut req = req.bearer_auth(token);
        if let (Some(id), Some(secret)) = (
            &self.credentials.cf_access_client_id,
            &self.credentials.cf_access_client_secret,
        ) {
            req = req
                .header("CF-Access-Client-Id", id)
                .header("CF-Access-Client-Secret", secret);
        }
        Ok(req)
    }
}

/// Default overall timeout of an authenticated client when the caller has
/// no better number (connect has its own 10 s).
pub const DEFAULT_AUTHED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// HTTP client that authorizes each request at send time. Mirrors the
/// `reqwest::Client` request surface the service clients use. `plain` wraps
/// an unauthenticated client (LAN servers) behind the same type.
#[derive(Clone)]
pub struct AuthedHttp {
    http: reqwest::Client,
    auth: Option<std::sync::Arc<AuthenticatedClient>>,
}

impl AuthedHttp {
    /// No authentication: requests go out exactly as built.
    pub fn plain(http: reqwest::Client) -> Self {
        Self { http, auth: None }
    }

    /// Cloud client: every request gets a fresh token. `timeout` is the
    /// overall per-request timeout (override per request with
    /// [`AuthedRequest::timeout`]).
    pub fn with_auth(
        auth: AuthenticatedClient,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let http = crate::http::client_builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(timeout)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            auth: Some(std::sync::Arc::new(auth)),
        })
    }

    fn req(&self, b: reqwest::RequestBuilder) -> AuthedRequest {
        AuthedRequest {
            builder: b,
            auth: self.auth.clone(),
        }
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> AuthedRequest {
        self.req(self.http.get(url))
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> AuthedRequest {
        self.req(self.http.post(url))
    }

    pub fn delete(&self, url: impl reqwest::IntoUrl) -> AuthedRequest {
        self.req(self.http.delete(url))
    }
}

/// A request being built; the token is attached in [`AuthedRequest::send`].
pub struct AuthedRequest {
    builder: reqwest::RequestBuilder,
    auth: Option<std::sync::Arc<AuthenticatedClient>>,
}

impl AuthedRequest {
    pub fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.builder = self.builder.json(body);
        self
    }

    pub fn multipart(mut self, form: reqwest::multipart::Form) -> Self {
        self.builder = self.builder.multipart(form);
        self
    }

    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.builder = self.builder.timeout(timeout);
        self
    }

    pub fn header(mut self, key: &str, value: impl AsRef<str>) -> Self {
        self.builder = self.builder.header(key, value.as_ref());
        self
    }

    /// Authorize (when the client is authenticated) and send. A token
    /// failure is returned as the error, never retried.
    pub async fn send(self) -> anyhow::Result<reqwest::Response> {
        let builder = match &self.auth {
            Some(auth) => auth.authorize(self.builder).await?,
            None => self.builder,
        };
        Ok(builder.send().await?)
    }
}

/// Check if a server URL appears to be a cloud gateway URL.
pub fn is_cloud_url(server_url: &str) -> bool {
    server_url.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> CloudCredentials {
        CloudCredentials {
            gateway_url: "https://gateway.example.local".into(),
            refresh_token: "refresh-token-value".into(),
            device_name: "laptop".into(),
            cf_access_client_id: Some("cf-id".into()),
            cf_access_client_secret: Some("cf-secret-value".into()),
        }
    }

    #[test]
    fn debug_redacts_refresh_token_and_cloudflare_secret() {
        let shown = format!("{:?}", creds());
        assert!(!shown.contains("refresh-token-value"), "{shown}");
        assert!(!shown.contains("cf-secret-value"), "{shown}");
        assert!(shown.contains("gateway.example.local"));
        assert!(shown.contains("laptop"));
    }

    #[test]
    fn save_then_load_roundtrips_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/cloud-token");
        creds().save(&path).unwrap();
        let loaded = CloudCredentials::load(&path).unwrap();
        assert_eq!(loaded.refresh_token, "refresh-token-value");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("cloud-token")]);
    }

    /// RA-78: the secret is never on disk with default-umask permissions,
    /// including when it replaces an older, world-readable file.
    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only_even_over_a_world_readable_predecessor() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-token");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        creds().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let fresh = dir.path().join("other-token");
        creds().save(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    use crate::auth::token::{create_token, now_epoch, TokenClaims};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn token(ttl: u64) -> String {
        let now = now_epoch();
        create_token(
            b"test-secret-0123456789abcdef0123456789",
            &TokenClaims {
                sub: "d".into(),
                iat: now,
                exp: now + ttl,
                scope: vec!["scribe".into()],
            },
        )
        .unwrap()
    }

    #[derive(Default)]
    struct Seen {
        refreshes: usize,
        auth_headers: Vec<String>,
    }

    /// Loopback gateway: `/cloud/refresh` answers from `refresh` (status,
    /// body) in order, every other path records its Authorization header.
    async fn gateway(refresh: Vec<(u16, String)>) -> (String, Arc<StdMutex<Seen>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(StdMutex::new(Seen::default()));
        let (s2, refresh) = (seen.clone(), Arc::new(StdMutex::new(refresh)));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (seen, refresh) = (s2.clone(), refresh.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (status, body) = if head.starts_with("POST /cloud/refresh") {
                        let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
                        g.refreshes += 1;
                        let mut r = refresh.lock().unwrap_or_else(|e| e.into_inner());
                        if r.is_empty() {
                            (500, String::new())
                        } else {
                            r.remove(0)
                        }
                    } else {
                        let auth = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                            .unwrap_or("")
                            .to_string();
                        seen.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .auth_headers
                            .push(auth);
                        (200, "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn authed(gw: &str) -> AuthedHttp {
        let mut c = creds();
        c.gateway_url = gw.to_string();
        AuthedHttp::with_auth(AuthenticatedClient::new(c).unwrap(), DEFAULT_AUTHED_TIMEOUT).unwrap()
    }

    /// RA-58: the token is fetched per request. The first token is about to
    /// expire, so the second request must go out with the refreshed one.
    #[tokio::test]
    async fn each_request_carries_the_current_token_across_a_refresh() {
        let (t1, t2) = (token(30), token(3600));
        let (gw, seen) = gateway(vec![
            (200, format!(r#"{{"access_token":"{t1}"}}"#)),
            (200, format!(r#"{{"access_token":"{t2}"}}"#)),
        ])
        .await;
        let http = authed(&gw);
        http.get(format!("{gw}/x")).send().await.unwrap();
        http.post(format!("{gw}/y"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        http.get(format!("{gw}/z")).send().await.unwrap();

        let g = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            g.refreshes, 2,
            "token near expiry is refreshed, a fresh one is reused"
        );
        assert_eq!(g.auth_headers.len(), 3);
        assert!(g.auth_headers[0].contains(&t1));
        assert!(g.auth_headers[1].contains(&t2));
        assert_ne!(g.auth_headers[0], g.auth_headers[1]);
        assert_eq!(g.auth_headers[1], g.auth_headers[2]);
    }

    #[tokio::test]
    async fn rejected_refresh_is_an_error_and_is_not_retried() {
        let (gw, seen) = gateway(vec![(401, "{}".into())]).await;
        let err = authed(&gw).get(format!("{gw}/x")).send().await.unwrap_err();
        assert!(err.downcast_ref::<RefreshRejected>().is_some(), "{err:#}");
        let g = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.refreshes, 1);
        assert!(
            g.auth_headers.is_empty(),
            "no request may go out unauthenticated"
        );
    }

    #[tokio::test]
    async fn plain_client_sends_no_authorization() {
        let (gw, seen) = gateway(vec![]).await;
        let http = AuthedHttp::plain(reqwest::Client::new());
        http.get(format!("{gw}/x")).send().await.unwrap();
        let g = seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.refreshes, 0);
        assert_eq!(g.auth_headers, vec![String::new()]);
    }
}
