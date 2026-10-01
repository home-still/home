//! Service registry — dynamic discovery of scribe, distill, and mcp servers.
//!
//! Servers register via POST /registry/register with a valid access token.
//! Clients query GET /registry/services to discover available servers.
//! Heartbeats keep entries fresh; stale entries are reaped periodically.
//!
//! An entry belongs to the device (token subject) that registered it; only that
//! device may overwrite, heartbeat, enable/disable or deregister it. The
//! number of entries is capped per device and overall, and the announced URL
//! must pass [`crate::backend_url::normalize_registrable`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hs_common::auth::token::{TokenClaims, TokenType};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::auth::{self, Rejection, SERVICES};
use crate::backend_url;
use crate::state::GatewayState;

/// How long before a service entry is considered stale (no heartbeat).
const DEFAULT_STALE_TIMEOUT_SECS: u64 = 90;

/// How often to reap stale entries from the registry.
const REAP_INTERVAL_SECS: u64 = 180;

/// Maximum allowed length for service_type and url fields.
const MAX_FIELD_LEN: usize = 512;

/// Most entries a single device may hold.
const MAX_ENTRIES_PER_DEVICE: usize = 16;

/// Most entries the registry holds in total.
const MAX_ENTRIES_TOTAL: usize = 256;

// ── Data Model ─────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ServiceEntry {
    pub service_type: String,
    pub url: String,
    pub device_name: String,
    pub enabled: bool,
    pub last_heartbeat: Instant,
    pub metadata: ServiceMetadata,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServiceMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The in-memory registry. Key = "{service_type}:{url}".
#[derive(Clone)]
pub struct ServiceRegistry {
    services: Arc<RwLock<HashMap<String, ServiceEntry>>>,
    stale_timeout: Duration,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceRegistry {
    pub fn new() -> Self {
        let registry = Self {
            services: Arc::new(RwLock::new(HashMap::new())),
            stale_timeout: Duration::from_secs(DEFAULT_STALE_TIMEOUT_SECS),
        };

        // Spawn background reaper for stale entries
        let services = Arc::clone(&registry.services);
        let timeout = registry.stale_timeout;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(REAP_INTERVAL_SECS));
            loop {
                interval.tick().await;
                let mut map = services.write().await;
                let reaped = reap_stale(&mut map, timeout);
                if reaped > 0 {
                    tracing::info!("Reaped {reaped} stale registry entries");
                }
            }
        });

        registry
    }

    fn key(service_type: &str, url: &str) -> String {
        format!("{service_type}:{url}")
    }

    /// Register (or refresh) an entry. An existing entry for the same
    /// service/URL can only be re-registered by its owner.
    pub async fn register(&self, entry: ServiceEntry) -> Result<(), RegisterError> {
        let key = Self::key(&entry.service_type, &entry.url);
        let mut services = self.services.write().await;
        reap_stale(&mut services, self.stale_timeout);

        if let Some(existing) = services.get(&key) {
            if existing.device_name != entry.device_name {
                return Err(RegisterError::NotOwner);
            }
        } else {
            let owned = services
                .values()
                .filter(|e| e.device_name == entry.device_name)
                .count();
            if owned >= MAX_ENTRIES_PER_DEVICE {
                return Err(RegisterError::DeviceLimit);
            }
            if services.len() >= MAX_ENTRIES_TOTAL {
                return Err(RegisterError::RegistryFull);
            }
        }
        services.insert(key, entry);
        Ok(())
    }

    /// Deregister a service. Only succeeds if the caller owns it.
    pub async fn deregister(
        &self,
        service_type: &str,
        url: &str,
        caller_device: &str,
    ) -> DeregisterResult {
        let key = Self::key(service_type, url);
        let mut services = self.services.write().await;
        match services.get(&key) {
            Some(entry) if entry.device_name == caller_device => {
                services.remove(&key);
                DeregisterResult::Removed
            }
            Some(_) => DeregisterResult::NotOwner,
            None => DeregisterResult::NotFound,
        }
    }

    /// Heartbeat a service. Only succeeds if the caller owns it.
    pub async fn heartbeat(
        &self,
        service_type: &str,
        url: &str,
        caller_device: &str,
    ) -> HeartbeatResult {
        let key = Self::key(service_type, url);
        let mut services = self.services.write().await;
        match services.get_mut(&key) {
            Some(entry) if entry.device_name == caller_device => {
                entry.last_heartbeat = Instant::now();
                HeartbeatResult::Ok
            }
            Some(_) => HeartbeatResult::NotOwner,
            None => HeartbeatResult::NotFound,
        }
    }

    /// Enable or disable a service. Only succeeds if the caller owns it.
    pub async fn set_enabled(
        &self,
        service_type: &str,
        url: &str,
        enabled: bool,
        caller_device: &str,
    ) -> SetEnabledResult {
        let key = Self::key(service_type, url);
        let mut services = self.services.write().await;
        match services.get_mut(&key) {
            Some(entry) if entry.device_name == caller_device => {
                entry.enabled = enabled;
                SetEnabledResult::Ok
            }
            Some(_) => SetEnabledResult::NotOwner,
            None => SetEnabledResult::NotFound,
        }
    }

    /// Drop every entry owned by `device`. Returns how many were removed.
    pub async fn remove_owned_by(&self, device: &str) -> usize {
        let mut services = self.services.write().await;
        let before = services.len();
        services.retain(|_, e| e.device_name != device);
        before - services.len()
    }

    /// Return all services, with staleness indicated.
    pub async fn list_all(&self) -> Vec<ServiceInfo> {
        let services = self.services.read().await;
        let now = Instant::now();
        let mut result: Vec<_> = services
            .values()
            .map(|e| ServiceInfo {
                service_type: e.service_type.clone(),
                url: e.url.clone(),
                device_name: e.device_name.clone(),
                enabled: e.enabled,
                healthy: now.duration_since(e.last_heartbeat) < self.stale_timeout,
                last_heartbeat_secs_ago: now.duration_since(e.last_heartbeat).as_secs(),
                metadata: e.metadata.clone(),
            })
            .collect();
        // Stable ordering: by type, then url
        result.sort_by(|a, b| (&a.service_type, &a.url).cmp(&(&b.service_type, &b.url)));
        result
    }

    /// Return healthy, enabled services of a given type (sorted for deterministic selection).
    pub async fn healthy_services(&self, service_type: &str) -> Vec<String> {
        let services = self.services.read().await;
        let now = Instant::now();
        let mut urls: Vec<String> = services
            .values()
            .filter(|e| {
                e.service_type == service_type
                    && e.enabled
                    && now.duration_since(e.last_heartbeat) < self.stale_timeout
            })
            .map(|e| e.url.clone())
            .collect();
        urls.sort();
        urls
    }
}

/// Drop entries that missed two stale windows. Returns how many were removed.
fn reap_stale(map: &mut HashMap<String, ServiceEntry>, stale_timeout: Duration) -> usize {
    let now = Instant::now();
    let before = map.len();
    map.retain(|_, e| now.duration_since(e.last_heartbeat) < stale_timeout * 2);
    before - map.len()
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// Another device already registered this service/URL.
    NotOwner,
    /// This device already holds the per-device maximum.
    DeviceLimit,
    /// The registry as a whole is full.
    RegistryFull,
}

pub enum DeregisterResult {
    Removed,
    NotOwner,
    NotFound,
}

pub enum HeartbeatResult {
    Ok,
    NotOwner,
    NotFound,
}

pub enum SetEnabledResult {
    Ok,
    NotOwner,
    NotFound,
}

// ── API Types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub service_type: String,
    pub url: String,
    #[serde(default)]
    pub metadata: ServiceMetadata,
}

#[derive(Deserialize)]
pub struct DeregisterRequest {
    pub service_type: String,
    pub url: String,
}

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    pub service_type: String,
    pub url: String,
}

#[derive(Deserialize)]
pub struct SetEnabledRequest {
    pub service_type: String,
    pub url: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct ServiceInfo {
    pub service_type: String,
    pub url: String,
    pub device_name: String,
    pub enabled: bool,
    pub healthy: bool,
    pub last_heartbeat_secs_ago: u64,
    pub metadata: ServiceMetadata,
}

#[derive(Serialize)]
pub struct ServicesResponse {
    pub services: Vec<ServiceInfo>,
}

// ── Validation ─────────────────────────────────────────────────

fn validate_service_type(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("service_type cannot be empty".into());
    }
    if s.len() > MAX_FIELD_LEN {
        return Err(format!("service_type exceeds max length ({MAX_FIELD_LEN})"));
    }
    if !SERVICES.contains(&s) {
        return Err(format!(
            "invalid service_type '{}', must be one of: {}",
            s,
            SERVICES.join(", ")
        ));
    }
    Ok(())
}

/// Validate and canonicalize an announced URL (see [`backend_url`] for exactly
/// what is refused). Every handler canonicalizes before keying the registry, so
/// `http://192.0.2.5:7433` and `http://192.0.2.5:7433/` are the same entry.
fn canonical_url(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("url cannot be empty".into());
    }
    if s.len() > MAX_FIELD_LEN {
        return Err(format!("url exceeds max length ({MAX_FIELD_LEN})"));
    }
    backend_url::normalize_registrable(s).map_err(|e| format!("invalid url: {e}"))
}

// ── Handlers ───────────────────────────────────────────────────

/// Authenticate an access token or produce the 401 response.
fn require_access(state: &GatewayState, headers: &HeaderMap) -> Result<TokenClaims, Rejection> {
    auth::authenticate(state, headers, TokenType::Access)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn bad_request(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, msg).into_response()
}

/// POST /registry/register — server announces itself.
pub async fn handle_register(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let claims = match require_access(&state, &headers) {
        Ok(c) => c,
        Err(rejection) => return rejection.into_response(),
    };

    if let Err(e) = validate_service_type(&req.service_type) {
        return bad_request(e);
    }
    let url = match canonical_url(&req.url) {
        Ok(u) => u,
        Err(e) => return bad_request(e),
    };

    // Token must have scope for the service being registered
    if !claims.has_scope(&req.service_type) {
        return (
            StatusCode::FORBIDDEN,
            format!("Token lacks scope: {}", req.service_type),
        )
            .into_response();
    }

    let entry = ServiceEntry {
        service_type: req.service_type.clone(),
        url: url.clone(),
        device_name: claims.sub.clone(),
        enabled: true,
        last_heartbeat: Instant::now(),
        metadata: req.metadata,
    };

    match state.registry.register(entry).await {
        Ok(()) => {
            tracing::info!(
                "Registered {}/{} from {}",
                req.service_type,
                url,
                claims.sub
            );
            (StatusCode::OK, "registered").into_response()
        }
        Err(RegisterError::NotOwner) => (
            StatusCode::FORBIDDEN,
            "Another device already registered this service",
        )
            .into_response(),
        Err(RegisterError::DeviceLimit) => (
            StatusCode::TOO_MANY_REQUESTS,
            format!("This device already has {MAX_ENTRIES_PER_DEVICE} registered services"),
        )
            .into_response(),
        Err(RegisterError::RegistryFull) => (
            StatusCode::TOO_MANY_REQUESTS,
            "The service registry is full",
        )
            .into_response(),
    }
}

/// DELETE /registry/deregister — server removes itself.
pub async fn handle_deregister(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<DeregisterRequest>,
) -> Response {
    let claims = match require_access(&state, &headers) {
        Ok(c) => c,
        Err(rejection) => return rejection.into_response(),
    };
    let url = match canonical_url(&req.url) {
        Ok(u) => u,
        Err(e) => return bad_request(e),
    };

    match state
        .registry
        .deregister(&req.service_type, &url, &claims.sub)
        .await
    {
        DeregisterResult::Removed => {
            tracing::info!(
                "Deregistered {}/{} from {}",
                req.service_type,
                url,
                claims.sub
            );
            (StatusCode::OK, "ok").into_response()
        }
        DeregisterResult::NotOwner => (
            StatusCode::FORBIDDEN,
            "Cannot deregister another device's service",
        )
            .into_response(),
        DeregisterResult::NotFound => (StatusCode::OK, "ok").into_response(), // idempotent
    }
}

/// POST /registry/heartbeat — server sends periodic heartbeat.
pub async fn handle_heartbeat(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> Response {
    let claims = match require_access(&state, &headers) {
        Ok(c) => c,
        Err(rejection) => return rejection.into_response(),
    };
    let url = match canonical_url(&req.url) {
        Ok(u) => u,
        Err(e) => return bad_request(e),
    };

    match state
        .registry
        .heartbeat(&req.service_type, &url, &claims.sub)
        .await
    {
        HeartbeatResult::Ok => (StatusCode::OK, "ok").into_response(),
        HeartbeatResult::NotOwner => (
            StatusCode::FORBIDDEN,
            "Cannot heartbeat another device's service",
        )
            .into_response(),
        HeartbeatResult::NotFound => {
            (StatusCode::NOT_FOUND, "service not registered").into_response()
        }
    }
}

/// POST /registry/set-enabled — enable or disable a server. Only the device
/// that registered the entry may do so.
pub async fn handle_set_enabled(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(req): Json<SetEnabledRequest>,
) -> Response {
    let claims = match require_access(&state, &headers) {
        Ok(c) => c,
        Err(rejection) => return rejection.into_response(),
    };
    let url = match canonical_url(&req.url) {
        Ok(u) => u,
        Err(e) => return bad_request(e),
    };

    match state
        .registry
        .set_enabled(&req.service_type, &url, req.enabled, &claims.sub)
        .await
    {
        SetEnabledResult::Ok => {
            let action = if req.enabled { "enabled" } else { "disabled" };
            tracing::info!("{} {}/{} by {}", action, req.service_type, url, claims.sub);
            (StatusCode::OK, "ok").into_response()
        }
        SetEnabledResult::NotOwner => (
            StatusCode::FORBIDDEN,
            "Cannot enable or disable another device's service",
        )
            .into_response(),
        SetEnabledResult::NotFound => {
            (StatusCode::NOT_FOUND, "service not registered").into_response()
        }
    }
}

/// GET /registry/services — client queries available servers.
pub async fn handle_services(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(rejection) = require_access(&state, &headers) {
        return rejection.into_response();
    }

    let services = state.registry.list_all().await;
    Json(ServicesResponse { services }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{body_json, call, test_state};
    use axum::body::Body;
    use axum::http::{Method, Request};

    const URL_A: &str = "http://192.0.2.10:7433";
    const URL_B: &str = "http://192.0.2.11:7433";

    fn token(state: &Arc<GatewayState>, sub: &str, scopes: &[&str], typ: TokenType) -> String {
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        auth::issue_token(state, sub, &scopes, typ).unwrap()
    }

    fn access(state: &Arc<GatewayState>, sub: &str) -> String {
        token(state, sub, &["scribe", "distill", "mcp"], TokenType::Access)
    }

    fn req(method: Method, path: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn register(
        state: &Arc<GatewayState>,
        bearer: &str,
        service: &str,
        url: &str,
    ) -> StatusCode {
        call(
            state,
            req(
                Method::POST,
                "/registry/register",
                bearer,
                serde_json::json!({ "service_type": service, "url": url }),
            ),
        )
        .await
        .status()
    }

    #[tokio::test]
    async fn device_registers_and_heartbeats_its_own_lan_url() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        assert_eq!(
            register(&state, &big, "scribe", URL_A).await,
            StatusCode::OK
        );

        let hb = call(
            &state,
            req(
                Method::POST,
                "/registry/heartbeat",
                &big,
                serde_json::json!({ "service_type": "scribe", "url": URL_A }),
            ),
        )
        .await;
        assert_eq!(hb.status(), StatusCode::OK);
        assert_eq!(state.registry.healthy_services("scribe").await, vec![URL_A]);
    }

    #[tokio::test]
    async fn trailing_slash_is_the_same_entry() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        assert_eq!(
            register(&state, &big, "scribe", "http://192.0.2.10:7433/").await,
            StatusCode::OK
        );
        assert_eq!(
            register(&state, &big, "scribe", URL_A).await,
            StatusCode::OK
        );
        assert_eq!(state.registry.list_all().await.len(), 1);
        assert_eq!(state.registry.healthy_services("scribe").await, vec![URL_A]);
    }

    #[tokio::test]
    async fn another_device_cannot_take_over_or_disable_an_entry() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        let mallory = access(&state, "mallory");
        assert_eq!(
            register(&state, &big, "scribe", URL_A).await,
            StatusCode::OK
        );

        // Overwrite attempt: refused, and the owner is unchanged.
        assert_eq!(
            register(&state, &mallory, "scribe", URL_A).await,
            StatusCode::FORBIDDEN
        );
        let listed = state.registry.list_all().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_name, "big");

        // Disable attempt: refused, and the entry is still routable.
        let denied = call(
            &state,
            req(
                Method::POST,
                "/registry/set-enabled",
                &mallory,
                serde_json::json!({ "service_type": "scribe", "url": URL_A, "enabled": false }),
            ),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.registry.healthy_services("scribe").await, vec![URL_A]);

        // Deregister attempt: refused.
        let denied = call(
            &state,
            req(
                Method::DELETE,
                "/registry/deregister",
                &mallory,
                serde_json::json!({ "service_type": "scribe", "url": URL_A }),
            ),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        // The owner can still manage it.
        let ok = call(
            &state,
            req(
                Method::POST,
                "/registry/set-enabled",
                &big,
                serde_json::json!({ "service_type": "scribe", "url": URL_A, "enabled": false }),
            ),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(state.registry.healthy_services("scribe").await.is_empty());
    }

    #[tokio::test]
    async fn ssrf_and_malformed_urls_are_rejected_at_registration() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        for url in [
            "http://127.0.0.1:7433",
            "http://169.254.169.254:80",
            "http://[::1]:7433",
            "http://0.0.0.0:7433",
            "http://user:pw@192.0.2.10:7433",
            "ftp://192.0.2.10:21",
            "file:///etc/passwd",
            "http://scribe.example.local:7433",
            "http://192.0.2.10:7433/admin",
            "",
        ] {
            assert_eq!(
                register(&state, &big, "scribe", url).await,
                StatusCode::BAD_REQUEST,
                "{url:?}"
            );
        }
        assert!(state.registry.list_all().await.is_empty());
    }

    #[tokio::test]
    async fn a_device_cannot_register_without_the_matching_scope_or_with_a_refresh_token() {
        let state = test_state(&[]).await;
        let scribe_only = token(&state, "big", &["scribe"], TokenType::Access);
        assert_eq!(
            register(&state, &scribe_only, "distill", URL_A).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            register(&state, &scribe_only, "scribe", URL_A).await,
            StatusCode::OK
        );

        let refresh = token(&state, "big", &["scribe"], TokenType::Refresh);
        assert_eq!(
            register(&state, &refresh, "scribe", URL_B).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            register(&state, "garbage", "scribe", URL_B).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn per_device_entry_cap_is_enforced_but_other_devices_are_unaffected() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        for port in 0..MAX_ENTRIES_PER_DEVICE {
            let url = format!("http://192.0.2.10:{}", 8000 + port);
            assert_eq!(register(&state, &big, "scribe", &url).await, StatusCode::OK);
        }
        assert_eq!(
            register(&state, &big, "scribe", "http://192.0.2.10:9999").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // Refreshing an existing entry is not growth.
        assert_eq!(
            register(&state, &big, "scribe", "http://192.0.2.10:8000").await,
            StatusCode::OK
        );
        let other = access(&state, "other");
        assert_eq!(
            register(&state, &other, "scribe", URL_B).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn revoking_a_device_removes_its_registry_entries() {
        let state = test_state(&[]).await;
        let big = access(&state, "big");
        let other = access(&state, "other");
        register(&state, &big, "scribe", URL_A).await;
        register(&state, &other, "scribe", URL_B).await;

        assert_eq!(state.registry.remove_owned_by("big").await, 1);
        assert_eq!(state.registry.healthy_services("scribe").await, vec![URL_B]);
    }

    #[tokio::test]
    async fn services_listing_requires_an_access_token() {
        let state = test_state(&[]).await;
        let resp = call(
            &state,
            Request::get("/registry/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let big = access(&state, "big");
        register(&state, &big, "mcp", URL_A).await;
        let resp = call(
            &state,
            Request::get("/registry/services")
                .header("authorization", format!("Bearer {big}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["services"][0]["device_name"], "big");
    }
}
