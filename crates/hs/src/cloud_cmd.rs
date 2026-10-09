//! `hs cloud` subcommand — manage remote cloud access.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use hs_common::auth::client::CloudCredentials;
use hs_common::auth::token;
use hs_common::reporter::Reporter;

/// Connection setup for requests to a gateway.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request ceiling for the small JSON calls this command makes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Subcommand, Debug)]
pub enum CloudCmd {
    /// Initialize this node as the cloud gateway
    Init,
    /// Generate a one-time enrollment code for a new device (run on the gateway host)
    Invite {
        /// Device name for the enrollment
        #[arg(long, default_value = "device")]
        name: String,
        /// Scope to grant (scribe, distill, mcp); repeat for several. Default: all three.
        #[arg(long = "scope", value_name = "SCOPE")]
        scopes: Vec<String>,
    },
    /// Revoke every token issued to a device (run on the gateway host)
    Revoke {
        /// Device name, or `oauth:<client_id>` for an OAuth client
        #[arg(long)]
        name: String,
    },
    /// Enroll this device with a cloud gateway
    Enroll {
        /// Gateway URL (e.g., https://cloud.example.com)
        #[arg(long)]
        gateway: String,
    },
    /// Show cloud connection status
    Status,
    /// Print a fresh access token (for MCP server config, CI, etc.)
    Token,
}

pub async fn dispatch(cmd: CloudCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        CloudCmd::Init => cmd_init(reporter).await,
        CloudCmd::Invite { name, scopes } => cmd_invite(&name, scopes, reporter).await,
        CloudCmd::Revoke { name } => cmd_revoke(&name, reporter).await,
        CloudCmd::Enroll { gateway } => cmd_enroll(&gateway, reporter).await,
        CloudCmd::Status => cmd_status(reporter).await,
        CloudCmd::Token => cmd_token(reporter).await,
    }
}

// ── Gateway config (gateway host only) ──────────────────────────

/// The parts of the `cloud.gateway` config section this command needs. The
/// gateway itself (`hs-gateway`) owns the full schema.
#[derive(Debug, PartialEq)]
struct GatewayLocal {
    /// `cloud.gateway.listen`, if the section sets it.
    listen: Option<String>,
    /// `cloud.gateway.secret_path`, or the default location.
    secret_path: PathBuf,
}

fn parse_gateway_local(yaml: &str) -> Result<GatewayLocal> {
    let root: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml).context("config is not valid YAML")?;
    let gateway = root.get("cloud").and_then(|cloud| cloud.get("gateway"));
    let text = |key: &str| {
        gateway
            .and_then(|g| g.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    Ok(GatewayLocal {
        listen: text("listen"),
        secret_path: text("secret_path")
            .map(PathBuf::from)
            .unwrap_or_else(token::default_secret_path),
    })
}

/// Read the gateway section of this host's config. A missing config file is
/// "nothing configured yet" (the state `hs cloud init` runs in); an unreadable
/// or malformed one is an error.
fn load_gateway_local() -> Result<GatewayLocal> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let path = home.join(hs_common::CONFIG_REL_PATH);
    match std::fs::read_to_string(&path) {
        Ok(yaml) => parse_gateway_local(&yaml).with_context(|| path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GatewayLocal {
            listen: None,
            secret_path: token::default_secret_path(),
        }),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The URL at which this host reaches its own gateway, from the configured
/// listen address: a wildcard bind (`0.0.0.0`, `::`) is reached on loopback.
fn local_admin_base(listen: &str) -> Result<String> {
    if let Ok(addr) = listen.parse::<SocketAddr>() {
        let ip = if addr.ip().is_unspecified() {
            match addr.ip() {
                IpAddr::V4(_) => IpAddr::from([127, 0, 0, 1]),
                IpAddr::V6(_) => IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
            }
        } else {
            addr.ip()
        };
        return Ok(match ip {
            IpAddr::V4(v4) => format!("http://{v4}:{}", addr.port()),
            IpAddr::V6(v6) => format!("http://[{v6}]:{}", addr.port()),
        });
    }
    // host:port, e.g. `localhost:7440`
    match listen.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            Ok(format!("http://{host}:{port}"))
        }
        _ => bail!("cloud.gateway.listen {listen:?} is not a host:port address"),
    }
}

/// Where this host's gateway listens and the admin key that authorizes
/// administrative calls to it. Only meaningful on the gateway host: the admin
/// key lives in a 0600 file beside the gateway's signing secret.
fn local_admin_target() -> Result<(String, String)> {
    let local = load_gateway_local()?;
    let listen = local.listen.context(
        "no `cloud.gateway.listen` in this host's config — run this on the gateway host",
    )?;
    let key_path = token::admin_key_path_for(&local.secret_path);
    let admin_key = token::load_admin_key(&key_path).with_context(|| {
        format!(
            "cannot read the gateway admin key at {} — is this the gateway host, and has \
             `hs cloud init` (or the gateway) created it?",
            key_path.display()
        )
    })?;
    Ok((local_admin_base(&listen)?, admin_key))
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

/// Client for the admin calls to this host's own gateway. It never goes
/// through an HTTP proxy from the environment: the admin key must not leave
/// the machine, and a proxy would also stamp the request with the forwarding
/// headers the gateway's admin endpoints refuse.
fn admin_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let local = load_gateway_local()?;
    let admin_path = token::admin_key_path_for(&local.secret_path);

    for (what, path, existed) in [
        (
            "Signing secret",
            &local.secret_path,
            local.secret_path.exists(),
        ),
        ("Admin key", &admin_path, admin_path.exists()),
    ] {
        if existed {
            reporter.status("Exists", &format!("{what} at {}", path.display()));
        } else {
            reporter.status("Creating", &format!("{what} at {}", path.display()));
        }
    }

    // One implementation, shared with the gateway: created 0600 from the
    // start; a malformed existing file is an error, never regenerated.
    token::load_or_create_secret(&local.secret_path)?;
    token::load_or_create_admin_key(&admin_path)?;

    reporter.status(
        "Next",
        "Add cloud.gateway config to ~/.home-still/config.yaml, then run `hs cloud invite`",
    );

    Ok(())
}

// ── Invite / Revoke ─────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct InviteResponse {
    code: String,
    expires_in_secs: u64,
    scopes: Vec<String>,
}

/// POST /cloud/admin/invite with the admin key.
async fn request_invite(
    http: &reqwest::Client,
    base: &str,
    admin_key: &str,
    device_name: &str,
    scopes: &[String],
) -> Result<InviteResponse> {
    let mut body = serde_json::json!({ "device_name": device_name });
    if !scopes.is_empty() {
        body["scopes"] = serde_json::json!(scopes);
    }

    let resp = http
        .post(format!("{base}/cloud/admin/invite"))
        .bearer_auth(admin_key)
        .json(&body)
        .send()
        .await
        .context("Is hs-gateway running? Start with: sudo systemctl start hs-gateway")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Failed to create enrollment ({status}): {body}");
    }
    resp.json().await.context("Invalid invite response")
}

async fn cmd_invite(
    device_name: &str,
    scopes: Vec<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let (base, admin_key) = local_admin_target()?;
    let body = request_invite(&admin_client()?, &base, &admin_key, device_name, &scopes).await?;

    reporter.status("Enrollment code", &body.code);
    reporter.status("Scopes", &body.scopes.join(", "));
    reporter.status(
        "Expires",
        &format!("in {}", human_duration(body.expires_in_secs)),
    );
    reporter.status(
        "Usage",
        &format!(
            "On the remote device, run:\n  hs cloud enroll --gateway <gateway-url>\n  then enter code: {}",
            body.code
        ),
    );

    Ok(())
}

async fn cmd_revoke(subject: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let (base, admin_key) = local_admin_target()?;

    let resp = admin_client()?
        .post(format!("{base}/cloud/admin/revoke"))
        .bearer_auth(&admin_key)
        .json(&serde_json::json!({ "subject": subject }))
        .send()
        .await
        .context("Is hs-gateway running? Start with: sudo systemctl start hs-gateway")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Failed to revoke ({status}): {body}");
    }

    reporter.status(
        "Revoked",
        &format!("{subject}; re-enroll it with `hs cloud invite`"),
    );
    Ok(())
}

// ── Enroll ──────────────────────────────────────────────────────

/// The gateway URL the user typed, as the bare https origin it must be. It is
/// what the credentials will be sent to for the rest of their life, so it is
/// never taken from anything the server says.
fn require_https_origin(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw.trim())
        .with_context(|| format!("gateway URL {raw:?} is not a valid URL"))?;
    if url.scheme() != "https" {
        bail!("gateway URL {raw:?} must use https (enrollment sends a credential to it)");
    }
    let Some(host) = url.host_str() else {
        bail!("gateway URL {raw:?} has no host");
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.path() != "/" && !url.path().is_empty())
    {
        bail!("gateway URL {raw:?} must be a bare origin (https://host[:port])");
    }
    Ok(match url.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

async fn cmd_enroll(gateway_url: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let gateway_url = require_https_origin(gateway_url)?;

    reporter.status("Gateway", &gateway_url);

    let code: String = dialoguer::Input::new()
        .with_prompt("Enrollment code")
        .interact()?;

    let http = http_client()?;

    let url = format!("{gateway_url}/cloud/enroll");
    reporter.status("Enrolling", "sending enrollment request...");

    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "code": code.trim(),
        }))
        .send()
        .await
        .context("Failed to reach gateway")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Enrollment failed ({status}): {body}");
    }

    #[derive(serde::Deserialize)]
    struct EnrollResponse {
        refresh_token: String,
        device_name: String,
    }

    let body: EnrollResponse = resp.json().await.context("Invalid enrollment response")?;

    let creds = CloudCredentials {
        gateway_url,
        refresh_token: body.refresh_token,
        device_name: body.device_name.clone(),
    };

    let cred_path = CloudCredentials::default_path()?;
    creds.save(&cred_path)?;

    reporter.status("Enrolled", &format!("as \"{}\"", body.device_name));
    reporter.status("Saved", &format!("{}", cred_path.display()));
    reporter.finish("Cloud access configured. Add the gateway URL to your scribe.servers config.");

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────

async fn cmd_status(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cred_path = CloudCredentials::default_path()?;

    if !cred_path.exists() {
        reporter.status("Cloud", "not enrolled");
        reporter.status(
            "Tip",
            "Run `hs cloud enroll --gateway <url>` to connect to a cloud gateway",
        );
        return Ok(());
    }

    let creds = CloudCredentials::load(&cred_path)?;
    reporter.status("Gateway", &creds.gateway_url);
    reporter.status("Device", &creds.device_name);

    // Try to refresh token to check connectivity
    let auth_client = hs_common::auth::client::AuthenticatedClient::new(creds)?;
    match auth_client.get_access_token().await {
        Ok(_) => reporter.status("Connection", "OK (token refreshed)"),
        Err(e) => reporter.warn(&format!("Connection failed: {e}")),
    }

    Ok(())
}

// ── Token ───────────────────────────────────────────────────────

async fn cmd_token(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cred_path = CloudCredentials::default_path()?;
    if !cred_path.exists() {
        anyhow::bail!("Not enrolled. Run `hs cloud enroll --gateway <url>` first.");
    }

    let creds = CloudCredentials::load(&cred_path)?;
    let auth_client = hs_common::auth::client::AuthenticatedClient::new(creds)?;
    let access_token = auth_client
        .get_access_token()
        .await
        .context("Failed to get access token")?;

    // The lifetime the gateway actually granted, read from the token itself.
    let claims = token::decode_claims_unverified(&access_token)
        .context("The gateway returned an access token this client cannot read")?;

    // Print just the token to stdout (for piping/copying)
    println!("{access_token}");
    reporter.status(
        "Expires",
        &format!("in {}", human_duration(claims.ttl_secs())),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn gateway_section_parsing_reads_listen_and_secret_path() {
        let local = parse_gateway_local(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    secret_path: /srv/hs/cloud-secret.key\n    routes: {}\n",
        )
        .unwrap();
        assert_eq!(local.listen.as_deref(), Some("127.0.0.1:7440"));
        assert_eq!(local.secret_path, PathBuf::from("/srv/hs/cloud-secret.key"));

        let bare = parse_gateway_local("home:\n  x: 1\n").unwrap();
        assert_eq!(bare.listen, None);
        assert_eq!(bare.secret_path, token::default_secret_path());

        assert!(parse_gateway_local("cloud: [unclosed").is_err());
    }

    #[test]
    fn admin_key_sits_beside_whatever_secret_path_the_config_names() {
        let local = parse_gateway_local(
            "cloud:\n  gateway:\n    listen: 127.0.0.1:7440\n    secret_path: /srv/hs/cloud-secret.key\n",
        )
        .unwrap();
        assert_eq!(
            token::admin_key_path_for(&local.secret_path),
            PathBuf::from("/srv/hs/cloud-admin.key")
        );
    }

    #[test]
    fn listen_address_maps_to_the_address_this_host_reaches_it_on() {
        for (listen, want) in [
            ("127.0.0.1:7440", "http://127.0.0.1:7440"),
            ("0.0.0.0:7440", "http://127.0.0.1:7440"),
            ("[::]:7440", "http://[::1]:7440"),
            ("[::1]:9000", "http://[::1]:9000"),
            ("192.0.2.5:7440", "http://192.0.2.5:7440"),
            ("localhost:7441", "http://localhost:7441"),
        ] {
            assert_eq!(local_admin_base(listen).unwrap(), want, "{listen}");
        }
        for bad in ["", "7440", "host:", "host:notaport", ":7440"] {
            assert!(local_admin_base(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn enroll_only_accepts_a_bare_https_origin() {
        assert_eq!(
            require_https_origin(" https://cloud.example.com/ ").unwrap(),
            "https://cloud.example.com"
        );
        assert_eq!(
            require_https_origin("https://cloud.example.com:8443").unwrap(),
            "https://cloud.example.com:8443"
        );
        for bad in [
            "http://cloud.example.com",
            "http://127.0.0.1:7440",
            "cloud.example.com",
            "ftp://cloud.example.com",
            "https://user:pw@cloud.example.com",
            "https://cloud.example.com/mcp",
            "https://cloud.example.com?next=https://evil.example",
            "",
        ] {
            assert!(require_https_origin(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn durations_print_as_what_they_are() {
        assert_eq!(human_duration(300), "5m");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(90), "1m 30s");
        assert_eq!(human_duration(14_400), "4h");
        assert_eq!(human_duration(12_600), "3h 30m");
        assert_eq!(human_duration(0), "0s");
    }

    /// One-shot HTTP server: records the raw request, answers `status`+`body`.
    async fn one_shot_server(
        status: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let want: usize = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= head_end + 4 + want {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            sock.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&raw).into_owned()
        });
        (base, handle)
    }

    #[tokio::test]
    async fn invite_authenticates_with_the_admin_key_and_reports_the_gateways_answer() {
        let (base, request) = one_shot_server(
            "200 OK",
            r#"{"code":"ABC-DEF","expires_in_secs":300,"scopes":["mcp"]}"#,
        )
        .await;

        let resp = request_invite(
            &admin_client().unwrap(),
            &base,
            "the-admin-key",
            "laptop",
            &["mcp".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(resp.code, "ABC-DEF");
        assert_eq!(resp.expires_in_secs, 300);

        let raw = request.await.unwrap().to_ascii_lowercase();
        assert!(raw.starts_with("post /cloud/admin/invite "), "{raw}");
        assert!(raw.contains("authorization: bearer the-admin-key"), "{raw}");
        assert!(raw.contains(r#""device_name":"laptop""#), "{raw}");
        assert!(raw.contains(r#""scopes":["mcp"]"#), "{raw}");
    }

    #[tokio::test]
    async fn invite_surfaces_a_gateway_refusal() {
        let (base, _request) = one_shot_server("401 Unauthorized", "Invalid admin key").await;
        let err = request_invite(&admin_client().unwrap(), &base, "wrong", "laptop", &[])
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("401") && msg.contains("Invalid admin key"),
            "{msg}"
        );
    }
}
