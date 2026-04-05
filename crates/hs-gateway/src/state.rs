//! Shared gateway state.

use crate::config::GatewayConfig;
use crate::enrollment::EnrollmentStore;

/// Shared state for the gateway server.
pub struct GatewayState {
    pub config: GatewayConfig,
    pub secret: Vec<u8>,
    pub http: reqwest::Client,
    pub enrollments: EnrollmentStore,
    /// Public gateway URL (for enrollment responses)
    pub gateway_url: String,
    /// Optional Cloudflare Access credentials to distribute during enrollment
    pub cf_access_client_id: Option<String>,
    pub cf_access_client_secret: Option<String>,
}
