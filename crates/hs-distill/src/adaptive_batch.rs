//! In-process adaptive batch-size controller for the ONNX embedder.
//!
//! Unlike Ollama — where NUM_PARALLEL changes require a process restart and
//! the tuning loop has to tolerate minutes of restart cost — fastembed's
//! batch_size is an in-process knob that can change between calls for free.
//! So this controller is NOT the restart-heavy daemon pattern of
//! `hs_scribe::ollama_tuner`; it's a lightweight EWMA-driven hill-climber
//! that reads throughput per-batch and adjusts on the fly.
//!
//! Algorithm matches `ollama_tuner::decide`'s shape (improvement threshold,
//! regression threshold, plateau-until-converged) because it's already
//! proven stable under real workload noise.

use std::sync::Mutex;

use super::embed::ComputeDevice;

#[derive(Debug, Clone)]
pub struct AdaptiveConfig {
    /// Candidate batch sizes in strictly ascending order. Must not be empty.
    pub candidates: Vec<usize>,
    /// Index into `candidates` where the controller starts.
    pub initial_idx: usize,
    /// How many observed batches between decision ticks.
    pub sample_interval: u32,
    /// Ratio of current EWMA over best that counts as an improvement.
    pub improvement_threshold: f64,
    /// Ratio of current EWMA over best that counts as a regression.
    pub regression_threshold: f64,
    /// Plateau ticks (within thresholds) before we call it converged and
    /// stop stepping.
    pub converge_after_stable: u32,
    /// EWMA smoothing factor in `[0, 1]`. Higher = faster to adapt; lower
    /// = steadier. `0.2` is a reasonable start.
    pub ewma_alpha: f64,
}

impl AdaptiveConfig {
    /// Device-default candidate set centered around a reasonable starting
    /// batch size. CUDA spans 16..128; CPU spans 4..24.
    pub fn default_for_device(device: &ComputeDevice, initial: usize) -> Self {
        let candidates: Vec<usize> = match device {
            ComputeDevice::Cuda => vec![16, 32, 48, 64, 96, 128],
            ComputeDevice::Cpu => vec![4, 8, 12, 16, 24],
        };
        // Snap `initial` to the closest candidate at or above it.
        let initial_idx = candidates
            .iter()
            .position(|&c| c >= initial)
            .unwrap_or(candidates.len() - 1);
        Self {
            candidates,
            initial_idx,
            sample_interval: 50,
            improvement_threshold: 1.05,
            regression_threshold: 0.90,
            converge_after_stable: 3,
            ewma_alpha: 0.2,
        }
    }

    /// Pin the controller to a single value (adaptive disabled). Observes
    /// throughput but never changes batch size.
    pub fn pinned(batch_size: usize) -> Self {
        Self {
            candidates: vec![batch_size],
            initial_idx: 0,
            sample_interval: u32::MAX,
            improvement_threshold: 1.05,
            regression_threshold: 0.90,
            converge_after_stable: 3,
            ewma_alpha: 0.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Noop,
    Stepped { from: usize, to: usize },
    Reverted { from: usize, to: usize },
    Converged { at: usize },
}

#[derive(Debug, Clone)]
struct State {
    current_idx: usize,
    best_idx: usize,
    best_rate: Option<f64>,
    /// +1 = step up through candidates, -1 = step down.
    direction: i32,
    samples_since_decision: u32,
    ewma_rate: Option<f64>,
    plateau_count: u32,
    converged: bool,
}

pub struct AdaptiveBatchController {
    state: Mutex<State>,
    cfg: AdaptiveConfig,
}

impl AdaptiveBatchController {
    pub fn new(cfg: AdaptiveConfig) -> Self {
        assert!(
            !cfg.candidates.is_empty(),
            "AdaptiveConfig::candidates must not be empty"
        );
        let start = cfg.initial_idx.min(cfg.candidates.len() - 1);
        Self {
            state: Mutex::new(State {
                current_idx: start,
                best_idx: start,
                best_rate: None,
                direction: 1,
                samples_since_decision: 0,
                ewma_rate: None,
                plateau_count: 0,
                converged: false,
            }),
            cfg,
        }
    }

    /// Current batch size callers should use.
    pub fn current(&self) -> usize {
        let s = self.state.lock().expect("AdaptiveBatchController poisoned");
        self.cfg.candidates[s.current_idx]
    }

    /// Record a completed embed batch. `batch_len` is the number of input
    /// texts; `elapsed_secs` is wall-clock. May change the current batch
    /// size if a decision tick has elapsed.
    pub fn observe(&self, batch_len: usize, elapsed_secs: f64) -> Decision {
        if batch_len == 0 || elapsed_secs <= 0.0 {
            return Decision::Noop;
        }
        let rate = batch_len as f64 / elapsed_secs;

        let mut s = self.state.lock().expect("AdaptiveBatchController poisoned");

        s.ewma_rate = Some(match s.ewma_rate {
            None => rate,
            Some(prev) => self.cfg.ewma_alpha * rate + (1.0 - self.cfg.ewma_alpha) * prev,
        });

        s.samples_since_decision += 1;
        if s.samples_since_decision < self.cfg.sample_interval {
            return Decision::Noop;
        }
        s.samples_since_decision = 0;

        if s.converged || self.cfg.candidates.len() == 1 {
            return Decision::Noop;
        }

        let current_rate = s.ewma_rate.expect("EWMA populated above");
        let from = self.cfg.candidates[s.current_idx];

        let Some(best) = s.best_rate else {
            // First decision: record current as best and step in the
            // default direction so we have two data points to compare.
            s.best_rate = Some(current_rate);
            s.best_idx = s.current_idx;
            let next = next_in_direction(s.current_idx, s.direction, self.cfg.candidates.len());
            if next != s.current_idx {
                s.current_idx = next;
                let to = self.cfg.candidates[s.current_idx];
                tracing::info!(
                    from,
                    to,
                    rate = current_rate,
                    "adaptive_batch: bootstrap step"
                );
                return Decision::Stepped { from, to };
            }
            // Boundary at bootstrap: flip direction so the next tick tries
            // the other way instead of re-hitting the same wall.
            s.direction = -s.direction;
            return Decision::Noop;
        };

        if current_rate >= best * self.cfg.improvement_threshold {
            // Improvement: update best and keep stepping.
            s.best_rate = Some(current_rate);
            s.best_idx = s.current_idx;
            s.plateau_count = 0;
            let next = next_in_direction(s.current_idx, s.direction, self.cfg.candidates.len());
            if next != s.current_idx {
                s.current_idx = next;
                let to = self.cfg.candidates[s.current_idx];
                tracing::info!(
                    from,
                    to,
                    rate = current_rate,
                    best,
                    "adaptive_batch: improvement, stepped"
                );
                return Decision::Stepped { from, to };
            }
            // Boundary — flip to search the other way next tick.
            s.direction = -s.direction;
            return Decision::Noop;
        }

        if current_rate <= best * self.cfg.regression_threshold {
            // Regression: revert to best and flip direction.
            s.current_idx = s.best_idx;
            s.direction = -s.direction;
            s.plateau_count = 0;
            let to = self.cfg.candidates[s.current_idx];
            tracing::warn!(
                from,
                to,
                rate = current_rate,
                best,
                "adaptive_batch: regression, reverted"
            );
            return Decision::Reverted { from, to };
        }

        // Plateau.
        s.plateau_count += 1;
        if s.plateau_count >= self.cfg.converge_after_stable {
            s.converged = true;
            let at = self.cfg.candidates[s.current_idx];
            tracing::info!(at, rate = current_rate, "adaptive_batch: converged");
            return Decision::Converged { at };
        }
        Decision::Noop
    }

    #[cfg(test)]
    pub fn snapshot_rate(&self) -> Option<f64> {
        self.state
            .lock()
            .expect("AdaptiveBatchController poisoned")
            .ewma_rate
    }
}

fn next_in_direction(idx: usize, dir: i32, len: usize) -> usize {
    if dir > 0 {
        if idx + 1 < len {
            idx + 1
        } else {
            idx
        }
    } else if idx > 0 {
        idx - 1
    } else {
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(candidates: Vec<usize>) -> AdaptiveConfig {
        AdaptiveConfig {
            candidates,
            initial_idx: 0,
            sample_interval: 2,
            improvement_threshold: 1.05,
            regression_threshold: 0.90,
            converge_after_stable: 3,
            ewma_alpha: 1.0, // identity EWMA for deterministic tests
        }
    }

    #[test]
    fn single_candidate_never_changes() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![64]));
        assert_eq!(ctrl.current(), 64);
        for _ in 0..10 {
            ctrl.observe(10, 1.0);
        }
        assert_eq!(ctrl.current(), 64);
    }

    #[test]
    fn bootstrap_steps_up_from_lowest() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![8, 16, 32]));
        assert_eq!(ctrl.current(), 8);
        // Two observations at rate 10 → bootstrap decision steps to idx 1
        ctrl.observe(10, 1.0);
        ctrl.observe(10, 1.0);
        assert_eq!(ctrl.current(), 16);
    }

    #[test]
    fn improvement_keeps_stepping() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![8, 16, 32]));
        ctrl.observe(10, 1.0);
        ctrl.observe(10, 1.0); // bootstrap → 16
        assert_eq!(ctrl.current(), 16);
        ctrl.observe(20, 1.0);
        ctrl.observe(20, 1.0); // rate 20 > 10*1.05 → improvement, step to 32
        assert_eq!(ctrl.current(), 32);
    }

    #[test]
    fn regression_reverts_and_flips() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![8, 16, 32]));
        ctrl.observe(10, 1.0);
        ctrl.observe(10, 1.0); // → 16 (best=10)
        ctrl.observe(20, 1.0);
        ctrl.observe(20, 1.0); // → 32 (best=20)
        ctrl.observe(5, 1.0);
        ctrl.observe(5, 1.0); // rate 5 < 20*0.90 → regression, revert to 16
        assert_eq!(ctrl.current(), 16);
        // Direction is now -1; further improvement should step down to 8.
        ctrl.observe(30, 1.0);
        ctrl.observe(30, 1.0);
        assert_eq!(ctrl.current(), 8);
    }

    #[test]
    fn plateau_converges() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![8, 16, 32]));
        ctrl.observe(10, 1.0);
        ctrl.observe(10, 1.0); // bootstrap → 16
                               // Three plateau decisions at rate 10 (within [9, 10.5]).
        for _ in 0..3 {
            ctrl.observe(10, 1.0);
            ctrl.observe(10, 1.0);
        }
        // Now converged at 16; a huge rate change shouldn't move us.
        ctrl.observe(1000, 1.0);
        ctrl.observe(1000, 1.0);
        assert_eq!(ctrl.current(), 16);
    }

    #[test]
    fn zero_duration_is_noop() {
        let ctrl = AdaptiveBatchController::new(cfg(vec![8, 16]));
        assert_eq!(ctrl.observe(10, 0.0), Decision::Noop);
        assert_eq!(ctrl.observe(0, 1.0), Decision::Noop);
        assert!(ctrl.snapshot_rate().is_none());
    }

    #[test]
    fn default_for_device_snaps_initial() {
        let cpu = AdaptiveConfig::default_for_device(&ComputeDevice::Cpu, 8);
        assert_eq!(cpu.candidates[cpu.initial_idx], 8);
        let cuda = AdaptiveConfig::default_for_device(&ComputeDevice::Cuda, 32);
        assert_eq!(cuda.candidates[cuda.initial_idx], 32);
        // Out-of-range initial snaps to next candidate up.
        let oddball = AdaptiveConfig::default_for_device(&ComputeDevice::Cpu, 10);
        assert_eq!(oddball.candidates[oddball.initial_idx], 12);
        // Above max → top candidate.
        let above = AdaptiveConfig::default_for_device(&ComputeDevice::Cpu, 999);
        assert_eq!(above.candidates[above.initial_idx], 24);
    }

    #[test]
    fn boundary_flips_direction_without_improvement() {
        // Start at top candidate with direction +1: can't step further up.
        let mut c = cfg(vec![8, 16, 32]);
        c.initial_idx = 2;
        let ctrl = AdaptiveBatchController::new(c);
        assert_eq!(ctrl.current(), 32);
        ctrl.observe(10, 1.0);
        ctrl.observe(10, 1.0); // bootstrap — boundary → flips to -1 silently
        ctrl.observe(20, 1.0);
        ctrl.observe(20, 1.0); // improvement with direction=-1 → step to 16
        assert_eq!(ctrl.current(), 16);
    }
}
