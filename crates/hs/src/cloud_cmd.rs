//! `hs cloud` subcommand — manage remote cloud access.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::auth::client::CloudCredentials;
use hs_common::auth::token;
use hs_common::reporter::Reporter;

#[derive(Subcommand, Debug)]
pub enum CloudCmd {
    /// Initialize this node as the cloud gateway
    Init,
    /// Generate a one-time enrollment code for a new device
    Invite {
        /// Device name for the enrollment
        #[arg(long, default_value = "device")]
        name: String,
    },
    /// Enroll this device with a cloud gateway
    Enroll {
        /// Gateway URL (e.g., https://cloud.lolzlab.com)
        #[arg(long)]
        gateway: String,
    },
    /// Show cloud connection status
    Status,
}

pub async fn dispatch(cmd: CloudCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        CloudCmd::Init => cmd_init(reporter).await,
        CloudCmd::Invite { name } => cmd_invite(&name, reporter).await,
        CloudCmd::Enroll { gateway } => cmd_enroll(&gateway, reporter).await,
        CloudCmd::Status => cmd_status(reporter).await,
    }
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let secret_path = home.join(hs_common::HIDDEN_DIR).join("cloud-secret.key");

    if secret_path.exists() {
        reporter.status("Exists", &format!("Secret at {}", secret_path.display()));
        reporter.status("Tip", "Delete the file to regenerate, or use as-is");
        return Ok(());
    }

    let secret = token::generate_secret();
    if let Some(parent) = secret_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&secret_path, &secret)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
    }

    reporter.status("Created", &format!("Secret at {}", secret_path.display()));
    reporter.status(
        "Next",
        "Add cloud.gateway config to ~/.home-still/config.yaml, then run `hs cloud invite`",
    );

    Ok(())
}

// ── Invite ──────────────────────────────────────────────────────

async fn cmd_invite(device_name: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    // POST to the running gateway's admin endpoint
    let gateway_local = "http://127.0.0.1:7440";

    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()?;

    let resp = http
        .post(format!("{gateway_local}/cloud/admin/invite"))
        .json(&serde_json::json!({
            "device_name": device_name,
            "scopes": ["scribe", "distill", "mcp"],
        }))
        .send()
        .await
        .context("Is hs-gateway running? Start with: sudo systemctl start hs-gateway")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to create enrollment: {body}");
    }

    #[derive(serde::Deserialize)]
    struct InviteResponse {
        code: String,
        expires_in_secs: u64,
    }

    let body: InviteResponse = resp.json().await?;

    reporter.status("Enrollment code", &body.code);
    reporter.status("Expires", &format!("{} seconds", body.expires_in_secs));
    reporter.status(
        "Usage",
        &format!(
            "On the remote device, run:\n  hs cloud enroll --gateway <gateway-url>\n  then enter code: {}",
            body.code
        ),
    );

    Ok(())
}

// ── Enroll ──────────────────────────────────────────────────────

async fn cmd_enroll(gateway_url: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let gateway_url = gateway_url.trim_end_matches('/');

    reporter.status("Gateway", gateway_url);

    let code: String = dialoguer::Input::new()
        .with_prompt("Enrollment code")
        .interact()?;

    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

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
        gateway_url: String,
        cf_access_client_id: Option<String>,
        cf_access_client_secret: Option<String>,
    }

    let body: EnrollResponse = resp.json().await.context("Invalid enrollment response")?;

    let creds = CloudCredentials {
        gateway_url: body.gateway_url,
        refresh_token: body.refresh_token,
        device_name: body.device_name.clone(),
        cf_access_client_id: body.cf_access_client_id,
        cf_access_client_secret: body.cf_access_client_secret,
    };

    let cred_path = CloudCredentials::default_path();
    creds.save(&cred_path)?;

    reporter.status("Enrolled", &format!("as \"{}\"", body.device_name));
    reporter.status("Saved", &format!("{}", cred_path.display()));
    reporter.finish("Cloud access configured. Add the gateway URL to your scribe.servers config.");

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────

async fn cmd_status(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cred_path = CloudCredentials::default_path();

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

    if creds.cf_access_client_id.is_some() {
        reporter.status("CF Access", "configured");
    }

    // Try to refresh token to check connectivity
    let auth_client = hs_common::auth::client::AuthenticatedClient::new(creds);
    match auth_client.get_access_token().await {
        Ok(_) => reporter.status("Connection", "OK (token refreshed)"),
        Err(e) => reporter.warn(&format!("Connection failed: {e}")),
    }

    Ok(())
}
