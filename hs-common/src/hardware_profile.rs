//! Hardware-class detection for per-host concurrency defaults.
//!
//! Different nodes in the home-still cluster have radically different
//! throughput ceilings: a Pi 5 saturates at NUM_PARALLEL=2, an RTX 4090
//! runs fine at 24+. Loading the same defaults everywhere means the Pi
//! spends ~90 min confirming it can't sustain NUM_PARALLEL=24 while the
//! RTX host never discovers that 32 or 48 would be faster. This module
//! detects the host class at startup and exposes the per-class defaults
//! that scribe/distill configs consume when the user hasn't overridden.
//!
//! Explicit config values (scribe.vlm_concurrency etc.) remain authoritative.
//! The profile only fills in defaults when the user hasn't set one.
//!
//! Override for tests / manual pinning: set `HS_HOST_CLASS` to one of
//! `pi | apple_low | apple_high | nvidia_mid | nvidia_high | generic_cpu`.

use std::sync::OnceLock;

/// What kind of box are we running on?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// ≤ 4 cores, no GPU, ≤ 8 GB RAM. Pi 5 / small ARM SBC.
    Pi,
    /// Apple Silicon, ≤ 16 GB unified memory. M1/M2 Air-class.
    AppleSiliconLow,
    /// Apple Silicon, > 16 GB unified memory. M1 Max / M3 Pro / M4 Pro class.
    AppleSiliconHigh,
    /// NVIDIA, 12–23 GB VRAM. RTX 3060 12GB / 4070 Ti / A4000 class.
    NvidiaMid,
    /// NVIDIA, ≥ 24 GB VRAM. RTX 3090 / 4090 / A5000+ class.
    NvidiaHigh,
    /// Linux / Windows CPU box without GPU, bigger than Pi. Fallback.
    GenericCpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuInfo {
    None,
    Nvidia { vram_mb: u64 },
    AppleSilicon,
}

#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub cpu_count: usize,
    pub ram_gb: u32,
    pub gpu: GpuInfo,
    pub class: HostClass,
}

impl HardwareProfile {
    /// Detect once per process; subsequent calls hit the cache. `nvidia-smi`
    /// and `sysctl` invocations are not repeated.
    pub fn detect() -> &'static HardwareProfile {
        static CACHE: OnceLock<HardwareProfile> = OnceLock::new();
        CACHE.get_or_init(detect_uncached)
    }
}

impl HostClass {
    /// Scribe VLM concurrency — inner Rust-layer throttle on concurrent
    /// VLM calls per scribe instance. See `hs-scribe::config::AppConfig::vlm_concurrency`.
    pub fn vlm_concurrency(self) -> usize {
        match self {
            HostClass::Pi => 2,
            HostClass::AppleSiliconLow => 4,
            HostClass::AppleSiliconHigh => 6,
            HostClass::NvidiaMid => 8,
            HostClass::NvidiaHigh => 12,
            HostClass::GenericCpu => 4,
        }
    }

    /// Scribe per-page region parallelism during per-region pipeline mode.
    /// See `hs-scribe::config::AppConfig::region_parallel`. Sized to keep
    /// `dispatcher_concurrency × page_parallel × region_parallel` close to
    /// the VLM backend's slot pool — overshoot causes prompt-cache
    /// eviction thrash that collapses eval throughput (see incident
    /// 2026-04-29: NvidiaHigh @ 6 fanned to 144+ concurrent calls
    /// against an 8-slot llama-server, eval rate dropped to 1.55 t/s).
    pub fn region_parallel(self) -> usize {
        match self {
            HostClass::Pi => 2,
            HostClass::AppleSiliconLow => 3,
            HostClass::AppleSiliconHigh => 4,
            HostClass::NvidiaMid => 3,
            HostClass::NvidiaHigh => 3,
            HostClass::GenericCpu => 2,
        }
    }

    /// Distill worker concurrency for the event-watch semaphore — how many
    /// markdown documents to embed in parallel on one distill host.
    pub fn distill_concurrency(self, cpu_count: usize) -> usize {
        match self {
            HostClass::Pi => 2,
            HostClass::AppleSiliconLow => 4,
            HostClass::AppleSiliconHigh => 6,
            HostClass::NvidiaMid | HostClass::NvidiaHigh => 8,
            HostClass::GenericCpu => (cpu_count / 4).max(2),
        }
    }

    /// Ollama NUM_PARALLEL candidate set the autotuner explores. Ordered
    /// strictly ascending; autotuner bootstraps from the lowest value.
    pub fn autotune_values(self) -> Vec<u32> {
        match self {
            HostClass::Pi => vec![1, 2, 3, 4],
            HostClass::AppleSiliconLow => vec![2, 4, 6, 8],
            HostClass::AppleSiliconHigh => vec![4, 6, 8, 12, 16],
            HostClass::NvidiaMid => vec![4, 8, 12, 16, 24, 32],
            HostClass::NvidiaHigh => vec![8, 12, 16, 24, 32, 48],
            HostClass::GenericCpu => vec![2, 4, 6, 8],
        }
    }

    /// Pool size for the layout/table ONNX detectors. Each detector costs
    /// ~100 MB resident, so sizing matches the host's memory budget.
    pub fn detector_pool_size(self) -> usize {
        match self {
            HostClass::Pi => 1,
            HostClass::AppleSiliconLow => 2,
            HostClass::AppleSiliconHigh => 4,
            HostClass::NvidiaMid => 4,
            HostClass::NvidiaHigh => 8,
            HostClass::GenericCpu => 2,
        }
    }

    fn from_str_override(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pi" => Some(HostClass::Pi),
            "apple_low" | "applesiliconlow" => Some(HostClass::AppleSiliconLow),
            "apple_high" | "applesiliconhigh" => Some(HostClass::AppleSiliconHigh),
            "nvidia_mid" | "nvidiamid" => Some(HostClass::NvidiaMid),
            "nvidia_high" | "nvidiahigh" => Some(HostClass::NvidiaHigh),
            "generic_cpu" | "genericcpu" => Some(HostClass::GenericCpu),
            _ => None,
        }
    }
}

fn detect_uncached() -> HardwareProfile {
    let cpu_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let ram_gb = detect_ram_gb();
    let gpu = detect_gpu();

    let class = if let Ok(env_override) = std::env::var("HS_HOST_CLASS") {
        HostClass::from_str_override(&env_override)
            .unwrap_or_else(|| classify(cpu_count, ram_gb, gpu))
    } else {
        classify(cpu_count, ram_gb, gpu)
    };

    HardwareProfile {
        cpu_count,
        ram_gb,
        gpu,
        class,
    }
}

fn classify(cpu_count: usize, ram_gb: u32, gpu: GpuInfo) -> HostClass {
    match gpu {
        GpuInfo::Nvidia { vram_mb } => {
            if vram_mb >= 24_000 {
                HostClass::NvidiaHigh
            } else {
                HostClass::NvidiaMid
            }
        }
        GpuInfo::AppleSilicon => {
            if ram_gb > 16 {
                HostClass::AppleSiliconHigh
            } else {
                HostClass::AppleSiliconLow
            }
        }
        GpuInfo::None => {
            if cpu_count <= 4 && ram_gb <= 8 {
                HostClass::Pi
            } else {
                HostClass::GenericCpu
            }
        }
    }
}

fn detect_gpu() -> GpuInfo {
    if let Some(vram_mb) = query_nvidia_vram_mb() {
        return GpuInfo::Nvidia { vram_mb };
    }
    if is_apple_silicon() {
        return GpuInfo::AppleSilicon;
    }
    GpuInfo::None
}

fn query_nvidia_vram_mb() -> Option<u64> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next()?.trim().parse::<u64>().ok()
}

fn is_apple_silicon() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let output = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok();
    match output {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).contains("Apple"),
        _ => false,
    }
}

fn detect_ram_gb() -> u32 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    if let Some(kb) = rest.split_whitespace().next() {
                        if let Ok(kb) = kb.parse::<u64>() {
                            return ((kb + 512 * 1024) / (1024 * 1024)) as u32;
                        }
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout);
                if let Ok(bytes) = s.trim().parse::<u64>() {
                    return ((bytes + 512 * 1024 * 1024) / (1024 * 1024 * 1024)) as u32;
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_pi_from_small_no_gpu() {
        assert_eq!(classify(4, 8, GpuInfo::None), HostClass::Pi);
    }

    #[test]
    fn classify_generic_from_big_no_gpu() {
        assert_eq!(classify(16, 32, GpuInfo::None), HostClass::GenericCpu);
    }

    #[test]
    fn classify_nvidia_mid_below_24gb() {
        assert_eq!(
            classify(12, 32, GpuInfo::Nvidia { vram_mb: 12_288 }),
            HostClass::NvidiaMid
        );
    }

    #[test]
    fn classify_nvidia_high_at_24gb() {
        assert_eq!(
            classify(24, 64, GpuInfo::Nvidia { vram_mb: 24_576 }),
            HostClass::NvidiaHigh
        );
    }

    #[test]
    fn classify_apple_silicon_low_at_16gb() {
        assert_eq!(
            classify(8, 16, GpuInfo::AppleSilicon),
            HostClass::AppleSiliconLow
        );
    }

    #[test]
    fn classify_apple_silicon_high_above_16gb() {
        assert_eq!(
            classify(12, 48, GpuInfo::AppleSilicon),
            HostClass::AppleSiliconHigh
        );
    }

    #[test]
    fn autotune_values_are_ascending() {
        for class in [
            HostClass::Pi,
            HostClass::AppleSiliconLow,
            HostClass::AppleSiliconHigh,
            HostClass::NvidiaMid,
            HostClass::NvidiaHigh,
            HostClass::GenericCpu,
        ] {
            let values = class.autotune_values();
            assert!(!values.is_empty(), "{class:?} has no autotune values");
            for pair in values.windows(2) {
                assert!(
                    pair[0] < pair[1],
                    "{class:?} autotune values must be strictly ascending, got {values:?}"
                );
            }
        }
    }

    #[test]
    fn distill_concurrency_scales_with_cores_on_generic() {
        assert_eq!(HostClass::GenericCpu.distill_concurrency(4), 2);
        assert_eq!(HostClass::GenericCpu.distill_concurrency(16), 4);
        assert_eq!(HostClass::GenericCpu.distill_concurrency(32), 8);
    }

    #[test]
    fn env_override_pins_class() {
        // can't set env vars safely in parallel tests; just verify the parser
        assert_eq!(HostClass::from_str_override("pi"), Some(HostClass::Pi));
        assert_eq!(
            HostClass::from_str_override("NVIDIA_HIGH"),
            Some(HostClass::NvidiaHigh)
        );
        assert_eq!(HostClass::from_str_override("bogus"), None);
    }
}
