//! `OLLAMA_NUM_PARALLEL` auto-tuner.
//!
//! Runs on a scribe host as a long-lived daemon (`hs scribe autotune`).
//! Each tick:
//!
//! 1. Sleep `warmup_secs` so the previous tick's Ollama restart and any
//!    in-flight converts have settled.
//! 2. Read the local scribe-server's `total_conversions` counter,
//!    sleep `measure_secs`, read it again. Delta / window = per-host
//!    throughput (conv/min).
//! 3. Decide:
//!    - rate ≥ best × `improvement_threshold` → step in current
//!      direction; new best.
//!    - rate ≤ best × `regression_threshold` → revert to best, flip
//!      direction.
//!    - otherwise plateau → bump a stable counter; once we've been flat
//!      for `converge_after_stable` ticks, mark converged and stop
//!      changing.
//! 4. If the decision is different from the current value, rewrite the
//!    platform's Ollama env-var location and restart. Persist state to
//!    `state_path`.
//!
//! The feedback loop is conservative by design: one restart per tick,
//! a full measurement window between changes, and a clear stop
//! condition. Tuning is persistent — on restart the tuner picks up the
//! last known-best and keeps ticking.
//!
//! Platform support:
//! - Linux: writes `/etc/systemd/system/ollama.service.d/num-parallel.conf`,
//!   runs `systemctl daemon-reload` + `systemctl restart ollama`. Must
//!   run as root.
//! - macOS: not yet wired (returns a clear `anyhow::Error`). Track per
//!   launcher (Homebrew / Desktop app / custom LaunchAgent) is a
//!   follow-up rc.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::client::{HealthResponse, ScribeClient};
use crate::config::AutotuneConfig;

/// Persisted tuner state. JSON round-trippable; survives daemon
/// restarts so we don't forget a hard-won best across reboots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// NUM_PARALLEL currently in effect.
    pub current_n: u32,
    /// Best observed (N, rate) so far.
    pub best_n: u32,
    pub best_rate: f64,
    /// +1 step up, -1 step down. Flipped on regression.
    pub direction: i32,
    /// Ticks in a row that landed in the "plateau" band.
    pub stable_count: u32,
    /// Once true, the tuner holds at `best_n` and stops stepping.
    pub converged: bool,
    /// Rolling history. Trimmed to the last 40 entries.
    pub history: Vec<Sample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub n: u32,
    pub rate_per_min: f64,
    pub converts: u64,
    pub ts: String,
}

impl State {
    fn bootstrap(starting_n: u32) -> Self {
        Self {
            current_n: starting_n,
            best_n: starting_n,
            best_rate: 0.0,
            direction: 1,
            stable_count: 0,
            converged: false,
            history: Vec::new(),
        }
    }

    fn load_or_bootstrap(path: &Path, starting_n: u32) -> Self {
        match std::fs::read_to_string(path) {
            Ok(txt) => match serde_json::from_str::<State>(&txt) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "state file unreadable — bootstrapping");
                    Self::bootstrap(starting_n)
                }
            },
            Err(_) => Self::bootstrap(starting_n),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Outcome of [`decide`]. `Noop` means stay at `current_n`; `Apply`
/// means restart Ollama with the new value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Noop,
    Apply(u32),
}

/// Pure hill-climb logic. No I/O. Takes a mutable `State` so the
/// caller can persist the post-decision state.
pub fn decide(cfg: &AutotuneConfig, state: &mut State, latest_rate: f64) -> Action {
    if state.converged {
        return if state.current_n == state.best_n {
            Action::Noop
        } else {
            state.current_n = state.best_n;
            Action::Apply(state.best_n)
        };
    }

    // Need at least one historical sample at current_n (the one we
    // just appended) — but on the very first tick we want to measure
    // once before making any change.
    let samples_at_current: Vec<f64> = state
        .history
        .iter()
        .filter(|s| s.n == state.current_n)
        .map(|s| s.rate_per_min)
        .collect();
    if samples_at_current.len() < 2 && state.best_rate == 0.0 {
        // Bootstrap: record the first sample as the best so we have
        // something to compare future ticks against.
        state.best_rate = latest_rate;
        state.best_n = state.current_n;
        // Step once in the default (+) direction to begin exploration.
        let next = next_in_direction(&cfg.values, state.current_n, state.direction);
        return step_to(state, next);
    }

    let avg_current = average_tail(&samples_at_current, 3);

    if avg_current >= state.best_rate * cfg.improvement_threshold {
        // Genuine improvement — record new best and keep stepping.
        state.best_n = state.current_n;
        state.best_rate = avg_current;
        state.stable_count = 0;
        let next = next_in_direction(&cfg.values, state.current_n, state.direction);
        return step_to(state, next);
    }

    if avg_current <= state.best_rate * cfg.regression_threshold {
        // Real regression — revert to best, flip direction.
        state.direction = -state.direction;
        state.stable_count = 0;
        return step_to(state, state.best_n);
    }

    // Plateau band.
    state.stable_count = state.stable_count.saturating_add(1);
    if state.stable_count >= cfg.converge_after_stable {
        state.converged = true;
        return step_to(state, state.best_n);
    }
    // Try one more nudge in the same direction.
    let next = next_in_direction(&cfg.values, state.current_n, state.direction);
    step_to(state, next)
}

fn step_to(state: &mut State, next_n: u32) -> Action {
    if next_n == state.current_n {
        Action::Noop
    } else {
        state.current_n = next_n;
        Action::Apply(next_n)
    }
}

fn average_tail(v: &[f64], tail: usize) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let take = tail.min(v.len());
    let slice = &v[v.len() - take..];
    slice.iter().copied().sum::<f64>() / slice.len() as f64
}

/// Return the next candidate value in `direction` (±1). Clamped at the
/// ends — at a boundary, returns the current value (caller treats that
/// as a no-op).
fn next_in_direction(values: &[u32], current: u32, direction: i32) -> u32 {
    if values.is_empty() {
        return current;
    }
    // Snap to nearest candidate if someone manually set an off-list value.
    let idx = match values.iter().position(|&v| v == current) {
        Some(i) => i,
        None => values
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| (**v as i64 - current as i64).abs())
            .map(|(i, _)| i)
            .unwrap_or(0),
    };
    let next_idx = idx as i32 + direction;
    if next_idx < 0 {
        values[0]
    } else if next_idx as usize >= values.len() {
        values[values.len() - 1]
    } else {
        values[next_idx as usize]
    }
}

async fn fetch_total_conversions(client: &ScribeClient) -> Result<u64> {
    let h: HealthResponse = client.health().await.context("scribe health probe")?;
    Ok(h.total_conversions)
}

/// Sample the counter at `t0` and again after `window`. Return
/// (conversions during window, rate per minute).
async fn measure_window(client: &ScribeClient, window: Duration) -> Result<(u64, f64)> {
    let t0 = fetch_total_conversions(client).await?;
    let start = Instant::now();
    tokio::time::sleep(window).await;
    let t1 = fetch_total_conversions(client).await?;
    let elapsed = start.elapsed().as_secs_f64().max(1.0);
    let converts = t1.saturating_sub(t0);
    let per_min = (converts as f64 / elapsed) * 60.0;
    Ok((converts, per_min))
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Apply a `NUM_PARALLEL` value by rewriting the platform-specific
/// Ollama config and restarting the service.
#[cfg(target_os = "linux")]
async fn apply_num_parallel(n: u32) -> Result<()> {
    let drop_in = PathBuf::from("/etc/systemd/system/ollama.service.d/num-parallel.conf");
    if let Some(parent) = drop_in.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let contents = format!("[Service]\nEnvironment=\"OLLAMA_NUM_PARALLEL={n}\"\n");
    std::fs::write(&drop_in, contents).with_context(|| format!("write {}", drop_in.display()))?;
    for args in [&["daemon-reload"][..], &["restart", "ollama"][..]] {
        let status = tokio::process::Command::new("systemctl")
            .args(args)
            .status()
            .await
            .with_context(|| format!("systemctl {}", args.join(" ")))?;
        if !status.success() {
            anyhow::bail!("systemctl {} failed", args.join(" "));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn apply_num_parallel(_n: u32) -> Result<()> {
    // macOS Ollama launchers vary per host — Homebrew services,
    // Ollama.app Desktop, or a custom LaunchAgent. A single code path
    // can't cover all three safely. Follow-up rc wires this up once
    // we standardise on one launcher per host.
    Err(anyhow::anyhow!(
        "autotune apply not yet implemented on macOS — set OLLAMA_NUM_PARALLEL manually for now"
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn apply_num_parallel(_n: u32) -> Result<()> {
    Err(anyhow::anyhow!(
        "autotune is only supported on Linux and macOS"
    ))
}

/// Long-running daemon. One tick per `tick_interval_secs`: warmup,
/// measure, decide, apply (maybe), persist state.
pub async fn run_forever(cfg: AutotuneConfig) -> Result<()> {
    if cfg.values.len() < 2 {
        anyhow::bail!("autotune.values must have at least 2 entries");
    }
    let mut sorted = cfg.values.clone();
    sorted.sort_unstable();
    if sorted != cfg.values {
        anyhow::bail!("autotune.values must be strictly increasing");
    }

    let starting_n = cfg.values.first().copied().unwrap_or(2);
    let mut state = State::load_or_bootstrap(&cfg.state_path, starting_n);
    let client = ScribeClient::new(&cfg.scribe_url);

    tracing::info!(
        starting_n = state.current_n,
        best_n = state.best_n,
        history_len = state.history.len(),
        scribe_url = %cfg.scribe_url,
        "autotune daemon starting"
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.tick_interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume immediate first tick

    loop {
        tracing::info!(current_n = state.current_n, "tick: warmup + measure");
        tokio::time::sleep(Duration::from_secs(cfg.warmup_secs)).await;
        let (converts, per_min) =
            match measure_window(&client, Duration::from_secs(cfg.measure_secs)).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "measure failed — skipping tick");
                    ticker.tick().await;
                    continue;
                }
            };

        state.history.push(Sample {
            n: state.current_n,
            rate_per_min: per_min,
            converts,
            ts: iso_now(),
        });
        if state.history.len() > 40 {
            let drop = state.history.len() - 40;
            state.history.drain(0..drop);
        }

        tracing::info!(
            converts,
            rate_per_min = per_min,
            current_n = state.current_n,
            best_n = state.best_n,
            best_rate = state.best_rate,
            "sample recorded"
        );

        let action = decide(&cfg, &mut state, per_min);
        match action {
            Action::Noop => {
                tracing::info!("no change (N={})", state.current_n);
            }
            Action::Apply(n) => {
                tracing::info!(new_n = n, "applying NUM_PARALLEL change");
                if let Err(e) = apply_num_parallel(n).await {
                    tracing::error!(error = ?e, n, "apply failed — reverting in-memory current_n to previous");
                    // Roll back the bookkeeping so next tick doesn't believe a
                    // failed apply succeeded.
                    state.current_n = state.history.iter().rev().map(|s| s.n).next().unwrap_or(n);
                }
            }
        }

        if let Err(e) = state.save(&cfg.state_path) {
            tracing::warn!(error = ?e, path = %cfg.state_path.display(), "state save failed");
        }

        ticker.tick().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> AutotuneConfig {
        AutotuneConfig {
            scribe_url: "http://127.0.0.1:7433".into(),
            tick_interval_secs: 1800,
            warmup_secs: 60,
            measure_secs: 600,
            values: vec![2, 4, 6, 8, 12, 16],
            improvement_threshold: 1.05,
            regression_threshold: 0.90,
            converge_after_stable: 3,
            state_path: PathBuf::from("/tmp/hs-autotune-test.json"),
        }
    }

    fn push_sample(state: &mut State, n: u32, rate: f64) {
        state.history.push(Sample {
            n,
            rate_per_min: rate,
            converts: 0,
            ts: "1970-01-01T00:00:00Z".into(),
        });
    }

    #[test]
    fn bootstrap_first_sample_steps_up() {
        let cfg = base_cfg();
        let mut state = State::bootstrap(4);
        push_sample(&mut state, 4, 10.0);
        let action = decide(&cfg, &mut state, 10.0);
        // First sample sets best_n=4, best_rate=10; steps to next higher.
        assert_eq!(action, Action::Apply(6));
        assert_eq!(state.current_n, 6);
        assert_eq!(state.best_n, 4);
    }

    #[test]
    fn improvement_steps_again() {
        let cfg = base_cfg();
        let mut state = State::bootstrap(4);
        state.best_n = 4;
        state.best_rate = 10.0;
        state.current_n = 6;
        push_sample(&mut state, 6, 11.0);
        push_sample(&mut state, 6, 11.2); // 2 samples at 6
        let action = decide(&cfg, &mut state, 11.2);
        // 11.1 avg >= 10 * 1.05 = 10.5 → improvement, step up to 8.
        assert_eq!(action, Action::Apply(8));
        assert_eq!(state.best_n, 6);
    }

    #[test]
    fn regression_reverts_and_flips() {
        let cfg = base_cfg();
        let mut state = State::bootstrap(6);
        state.best_n = 6;
        state.best_rate = 10.0;
        state.current_n = 8;
        state.direction = 1;
        push_sample(&mut state, 8, 8.0);
        push_sample(&mut state, 8, 8.2); // avg 8.1 ≤ 10 * 0.90 = 9.0
        let action = decide(&cfg, &mut state, 8.2);
        assert_eq!(action, Action::Apply(6));
        assert_eq!(state.direction, -1);
        assert_eq!(state.current_n, 6);
    }

    #[test]
    fn plateau_counts_stable_then_converges() {
        let cfg = base_cfg();
        let mut state = State::bootstrap(6);
        state.best_n = 6;
        state.best_rate = 10.0;
        state.current_n = 6;
        // Plateau samples: push at whatever `current_n` is on each tick,
        // since decide() may have advanced it on the previous iteration.
        for _ in 0..=cfg.converge_after_stable {
            let n = state.current_n;
            push_sample(&mut state, n, 10.0);
            let _ = decide(&cfg, &mut state, 10.0);
        }
        assert!(state.converged, "should converge after stable ticks");
    }

    #[test]
    fn next_in_direction_clamps_at_bounds() {
        let values = vec![2, 4, 8, 16];
        assert_eq!(next_in_direction(&values, 16, 1), 16);
        assert_eq!(next_in_direction(&values, 2, -1), 2);
        assert_eq!(next_in_direction(&values, 4, 1), 8);
        assert_eq!(next_in_direction(&values, 4, -1), 2);
        // Off-list value snaps to nearest candidate.
        assert_eq!(next_in_direction(&values, 5, 1), 8);
    }

    #[test]
    fn state_roundtrips_json() {
        let mut s = State::bootstrap(4);
        push_sample(&mut s, 4, 3.0);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        s.save(tmp.path()).unwrap();
        let loaded = State::load_or_bootstrap(tmp.path(), 2);
        assert_eq!(loaded.current_n, s.current_n);
        assert_eq!(loaded.history.len(), 1);
    }
}
