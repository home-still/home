//! Service registry — dynamic discovery of scribe, distill, and mcp servers.
//!
//! Servers register via POST /registry/register with a valid bearer token.
//! Clients query GET /registry/services to discover available servers.
//! Heartbeats keep entries fresh; stale entries are reaped periodically.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use hs_common::auth::token::{self, TokenError};

use crate::state::GatewayState;

/// How long before a service entry is considered stale (no heartbeat).
const DEFAULT_STALE_TIMEOUT_SECS: u64 = 90;

/// How often to reap stale entries from the registry.
const REAP_INTERVAL_SECS: u64 = 180;

/// Maximum allowed length for service_type and url fields.
const MAX_FIELD_LEN: usize = 512;

/// Allowed service types.
const VALID_SERVICE_TYPES: &[&str] = &["scribe", "distill", "mcp"];

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
                let now = Instant::now();
                let before = map.len();
                map.retain(|_, e| now.duration_since(e.last_heartbeat) < timeout * 2);
                let reaped = before - map.len();
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

    pub async fn register(&self, entry: ServiceEntry) {
        let key = Self::key(&entry.service_type, &entry.url);
        self.services.write().await.insert(key, entry);
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

    pub async fn set_enabled(&self, service_type: &str, url: &str, enabled: bool) -> bool {
        let key = Self::key(service_type, url);
        let mut services = self.services.write().await;
        if let Some(entry) = services.get_mut(&key) {
            entry.enabled = enabled;
            true
        } else {
            false
        }
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
    if !VALID_SERVICE_TYPES.contains(&s) {
        return Err(format!(
            "invalid service_type '{}', must be one of: {}",
            s,
            VALID_SERVICE_TYPES.join(", ")
        ));
    }
    Ok(())
}

fn validate_url(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("url cannot be empty".into());
    }
    if s.len() > MAX_FIELD_LEN {
        return Err(format!("url exceeds max length ({MAX_FIELD_LEN})"));
    }
    if !s.starts_with("http://") && !s.starts_with("https://") {
        return Err("url must start with http:// or https://".into());
    }
    Ok(())
}

// ── Handlers ───────────────────────────────────────────────────

/// Extract and validate a bearer token from the request headers.
fn extract_bearer(
    headers: &axum::http::HeaderMap,
    secret: &[u8],
) -> Result<token::TokenClaims, StatusCode> {
    let token_str = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    token::validate_token(secret, token_str, false).map_err(|e| match e {
        TokenError::Expired => StatusCode::UNAUTHORIZED,
        _ => StatusCode::UNAUTHORIZED,
    })
}

/// POST /registry/register — server announces itself.
pub async fn handle_register(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let claims = match extract_bearer(&headers, &state.secret) {
        Ok(c) => c,
        Err(status) => return (status, "Unauthorized").into_response(),
    };

    if let Err(e) = validate_service_type(&req.service_type) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    if let Err(e) = validate_url(&req.url) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

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
        url: req.url.clone(),
        device_name: claims.sub.clone(),
        enabled: true,
        last_heartbeat: Instant::now(),
        metadata: req.metadata,
    };

    state.registry.register(entry).await;
    tracing::info!(
        "Registered {}/{} from {}",
        req.service_type,
        req.url,
        claims.sub
    );

    (StatusCode::OK, "registered").into_response()
}

/// DELETE /registry/deregister — server removes itself.
pub async fn handle_deregister(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<DeregisterRequest>,
) -> impl IntoResponse {
    let claims = match extract_bearer(&headers, &state.secret) {
        Ok(c) => c,
        Err(status) => return (status, "Unauthorized").into_response(),
    };

    match state
        .registry
        .deregister(&req.service_type, &req.url, &claims.sub)
        .await
    {
        DeregisterResult::Removed => {
            tracing::info!(
                "Deregistered {}/{} from {}",
                req.service_type,
                req.url,
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
    headers: axum::http::HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> impl IntoResponse {
    let claims = match extract_bearer(&headers, &state.secret) {
        Ok(c) => c,
        Err(status) => return (status, "Unauthorized").into_response(),
    };

    match state
        .registry
        .heartbeat(&req.service_type, &req.url, &claims.sub)
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

/// POST /registry/set-enabled — enable or disable a server.
pub async fn handle_set_enabled(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SetEnabledRequest>,
) -> impl IntoResponse {
    let claims = match extract_bearer(&headers, &state.secret) {
        Ok(c) => c,
        Err(status) => return (status, "Unauthorized").into_response(),
    };

    // set-enabled is an admin action — any authenticated user with the right scope can do it
    if !claims.has_scope(&req.service_type) {
        return (
            StatusCode::FORBIDDEN,
            format!("Token lacks scope: {}", req.service_type),
        )
            .into_response();
    }

    let found = state
        .registry
        .set_enabled(&req.service_type, &req.url, req.enabled)
        .await;
    if found {
        let action = if req.enabled { "enabled" } else { "disabled" };
        tracing::info!(
            "{} {}/{} by {}",
            action,
            req.service_type,
            req.url,
            claims.sub
        );
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::NOT_FOUND, "service not registered").into_response()
    }
}

/// GET /registry/services — client queries available servers.
pub async fn handle_services(
    State(state): State<Arc<GatewayState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let _claims = match extract_bearer(&headers, &state.secret) {
        Ok(c) => c,
        Err(status) => return (status, "Unauthorized").into_response(),
    };

    let services = state.registry.list_all().await;
    Json(ServicesResponse { services }).into_response()
}
