//! Admission probe for the VLM backend behind `/scribe`.
//!
//! The scribe server does not own the GPU it dispatches to: llama-swap
//! (and whatever foreign tenant is resident) does. Before this module,
//! `/health` reported `ok` whenever the process was alive, so the
//! dispatcher happily sent conversions into a backend that could only
//! sit in llama-swap's `healthCheckTimeout` and then fail. The probe
//! answers one question — *can this backend start our model right now?*
//! — and the health/readiness handlers refuse work when it cannot.
//!
//! Two ways a cold start is viable:
//!   1. the model is already resident (llama-swap `/running` names it), or
//!   2. the card has at least `headroom_mb` free for llama-swap to load it.
//!
//! Hosts without a working `nvidia-smi` (Apple Silicon pool members)
//! have no VRAM signal, so the VRAM half of the gate is skipped there —
//! never disable a pool member for a measurement we cannot take.

use std::time::Duration;

/// Verdict for one backend at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendState {
    /// llama-swap answered `GET /running`.
    pub reachable: bool,
    /// Our model id appears in the `/running` set.
    pub model_resident: bool,
    /// `model_resident || free_vram_mb >= headroom_mb` (VRAM half
    /// skipped when the host exposes no NVIDIA GPU).
    pub can_start: bool,
    pub free_vram_mb: Option<u64>,
    /// RFC 3339, millisecond precision, UTC.
    pub checked_at: String,
}

impl BackendState {
    /// The single admission verdict both `/health` and `/readiness` use.
    pub fn admits(&self) -> bool {
        self.reachable && self.can_start
    }

    fn unreachable(free_vram_mb: Option<u64>) -> Self {
        Self {
            reachable: false,
            model_resident: false,
            can_start: false,
            free_vram_mb,
            checked_at: now_rfc3339(),
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Turn an OpenAI-compatible endpoint into the llama-swap admin base:
/// `http://localhost:8081/v1` → `http://localhost:8081`. Idempotent for
/// endpoints already given without the `/v1` suffix.
pub fn swap_base(olmocr_endpoint: &str) -> String {
    let trimmed = olmocr_endpoint.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

/// One entry of llama-swap v229's `GET /running`. Verified shape on
/// `big` 2026-09-21:
/// `{"running":[{"model":"glm-ocr","state":"ready","cmd":"…","proxy":"…","ttl":60,…}]}`.
/// Only `model` is read; the rest is llama-swap's business. A shape
/// change makes this deserialize to nothing, which reads as "not
/// resident" and defers to the free-VRAM half of the gate — strictly
/// the safe direction.
#[derive(serde::Deserialize)]
struct RunningEntry {
    model: String,
}

/// Whether `model` is currently loaded according to a `/running` body.
fn running_contains(body: &serde_json::Value, model: &str) -> bool {
    let Some(items) = body.get("running").and_then(|v| v.as_array()) else {
        return false;
    };
    items
        .iter()
        .filter_map(|item| serde_json::from_value::<RunningEntry>(item.clone()).ok())
        .any(|e| e.model == model)
}

/// Probe the backend once. Never returns an error: an unreachable
/// backend is a verdict (`reachable: false`), not a failure of the
/// health handler.
pub async fn probe(endpoint: &str, model: &str, headroom_mb: u64) -> BackendState {
    let free_vram_mb = hs_common::gpu::free_vram_mb();
    let url = format!("{}/running", swap_base(endpoint));

    let client = match hs_common::http::client_builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to build backend probe client");
            return BackendState::unreachable(free_vram_mb);
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return BackendState::unreachable(free_vram_mb),
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return BackendState::unreachable(free_vram_mb);
    };

    let model_resident = running_contains(&body, model);
    // No nvidia-smi ⇒ no VRAM gate on this host (Apple Silicon pool
    // members must stay dispatchable).
    let vram_ok = free_vram_mb.is_none_or(|free| free >= headroom_mb);

    BackendState {
        reachable: true,
        model_resident,
        can_start: model_resident || vram_ok,
        free_vram_mb,
        checked_at: now_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn swap_base_strips_v1_and_trailing_slash() {
        assert_eq!(
            swap_base("http://localhost:8081/v1"),
            "http://localhost:8081"
        );
        assert_eq!(
            swap_base("http://localhost:8081/v1/"),
            "http://localhost:8081"
        );
        assert_eq!(swap_base("http://localhost:8081"), "http://localhost:8081");
        assert_eq!(swap_base("http://localhost:8081/"), "http://localhost:8081");
    }

    #[test]
    fn swap_base_keeps_non_v1_path_prefix() {
        // A reverse-proxied endpoint must not lose its mount point.
        assert_eq!(
            swap_base("http://gateway.local/llama/v1"),
            "http://gateway.local/llama"
        );
    }

    #[test]
    fn empty_running_set_is_not_resident() {
        assert!(!running_contains(&json!({"running": []}), "olmocr"));
    }

    #[test]
    fn matches_the_verified_llama_swap_shape() {
        // Captured verbatim from `curl localhost:8081/running` on big,
        // llama-swap v229, 2026-09-21.
        assert!(running_contains(
            &json!({"running": [{
                "model": "glm-ocr",
                "state": "ready",
                "cmd": "bash /home/ladvien/.home-still/run-glm-ocr.sh 5801",
                "proxy": "http://127.0.0.1:5801",
                "ttl": 60,
                "name": "",
                "description": ""
            }]}),
            "glm-ocr"
        ));
    }

    #[test]
    fn only_the_model_field_counts() {
        // A model id that collides with some other field's value must
        // not read as resident.
        assert!(!running_contains(
            &json!({"running": [{"model": "glm-ocr", "state": "ready"}]}),
            "ready"
        ));
    }

    #[test]
    fn other_model_resident_does_not_count() {
        assert!(!running_contains(
            &json!({"running": [{"model": "qwen3-vl"}]}),
            "olmocr"
        ));
    }

    #[test]
    fn missing_running_key_is_not_resident() {
        assert!(!running_contains(&json!({"models": ["olmocr"]}), "olmocr"));
    }

    #[test]
    fn admits_requires_reachable_and_can_start() {
        let base = BackendState {
            reachable: true,
            model_resident: false,
            can_start: true,
            free_vram_mb: Some(20000),
            checked_at: now_rfc3339(),
        };
        assert!(base.admits());
        assert!(!BackendState {
            reachable: false,
            ..base.clone()
        }
        .admits());
        assert!(!BackendState {
            can_start: false,
            ..base
        }
        .admits());
    }
}
