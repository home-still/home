//! Shared gateway state.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::auth::SigningKeys;
use crate::config::GatewayConfig;
use crate::enrollment::{self, EnrollmentStore};
use crate::oauth::{self, AuthCodeStore, ClientStore};
use crate::ratelimit::RateLimits;
use crate::registry::ServiceRegistry;
use crate::revocation::Revocations;

/// Shared state for the gateway server.
pub struct GatewayState {
    pub config: GatewayConfig,
    pub keys: SigningKeys,
    /// Credential for the admin endpoints (`/cloud/admin/*`).
    pub admin_key: String,
    pub revocations: Revocations,
    pub http: reqwest::Client,
    pub enrollments: EnrollmentStore,
    /// Public https origin of the gateway (OAuth issuer / metadata)
    pub gateway_url: String,
    /// OAuth authorization codes pending exchange
    pub auth_codes: AuthCodeStore,
    /// Dynamically registered OAuth clients
    pub oauth_clients: ClientStore,
    /// Dynamic service registry (scribe, distill, mcp servers)
    pub registry: ServiceRegistry,
    pub rate_limits: RateLimits,
    /// One permit per proxied request in flight (held until the response body
    /// has been fully streamed or dropped).
    pub proxy_permits: Arc<Semaphore>,
}

impl GatewayState {
    /// Build the state. Must run inside a tokio runtime (the registry spawns
    /// its reaper).
    pub fn new(
        config: GatewayConfig,
        keys: SigningKeys,
        admin_key: String,
        revocations: Revocations,
        gateway_url: String,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(config.backend_connect_timeout_secs))
            .read_timeout(Duration::from_secs(config.backend_read_timeout_secs))
            .timeout(Duration::from_secs(config.backend_total_timeout_secs))
            // A reverse proxy hands a backend's redirect to the client; it must
            // not chase it into the LAN on the client's behalf.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            rate_limits: RateLimits::per_minute(config.auth_rate_limit_per_minute),
            proxy_permits: Arc::new(Semaphore::new(config.max_concurrent_proxy_requests)),
            enrollments: enrollment::new_enrollment_store(),
            auth_codes: oauth::new_auth_code_store(),
            oauth_clients: oauth::new_client_store(),
            registry: ServiceRegistry::new(),
            config,
            keys,
            admin_key,
            revocations,
            http,
            gateway_url,
        })
    }
}
