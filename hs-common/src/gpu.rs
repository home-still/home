//! Single source of truth for NVIDIA GPU state.
//!
//! Every home-still GPU consumer (scribe's VLM admission gate, distill's
//! embedder load gate, `/health` reporting) reads the card through this
//! module. One `nvidia-smi` shell-out per query, `None`/empty on every
//! failure so non-NVIDIA hosts (Apple Silicon pool members, Pis) keep a
//! clean response instead of a fabricated zero.
//!
//! Whole-card `memory_used_mb` is NOT a residency signal under
//! co-tenancy: on a contended card it reflects other processes'
//! allocations. Use [`self_vram_mb`] when the question is "did *my*
//! process get GPU memory?" and [`free_vram_mb`] when the question is
//! "can I still fit?".

use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub utilization_pct: Option<f32>,
    pub memory_used_mb: Option<u64>,
    pub memory_free_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
}

/// One CUDA process resident on the card, as reported by
/// `nvidia-smi --query-compute-apps`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeApp {
    pub pid: u32,
    pub used_mb: u64,
    pub name: String,
}

/// Run `nvidia-smi` with the given query args, returning trimmed stdout.
/// `None` when the binary is absent or exits non-zero.
fn nvidia_smi(args: &[&str]) -> Option<String> {
    let out = Command::new("nvidia-smi").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Shell out to nvidia-smi once and parse the first GPU's name,
/// utilization %, and memory used / free / total in MiB. All fields
/// `None` when nvidia-smi is absent or fails.
pub fn query_gpu_info() -> GpuInfo {
    let Some(stdout) = nvidia_smi(&[
        "--query-gpu=name,utilization.gpu,memory.used,memory.free,memory.total",
        "--format=csv,noheader,nounits",
    ]) else {
        return GpuInfo::default();
    };
    parse_gpu_info(&stdout)
}

fn parse_gpu_info(stdout: &str) -> GpuInfo {
    let line = stdout.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return GpuInfo::default();
    }
    let mut parts = line.split(',').map(str::trim);
    GpuInfo {
        name: parts.next().filter(|s| !s.is_empty()).map(String::from),
        utilization_pct: parts.next().and_then(|s| s.parse::<f32>().ok()),
        memory_used_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
        memory_free_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
        memory_total_mb: parts.next().and_then(|s| s.parse::<u64>().ok()),
    }
}

/// Free VRAM in MiB on the first GPU. `None` on hosts without a working
/// `nvidia-smi` — callers MUST treat that as "no gate to apply", never
/// as zero free.
pub fn free_vram_mb() -> Option<u64> {
    query_gpu_info().memory_free_mb
}

/// VRAM in MiB attributed to *this* process. `None` when nvidia-smi is
/// unavailable; `Some(0)` when the card is visible but this process
/// holds no allocation (the CPU-fallback signal).
pub fn self_vram_mb() -> Option<u64> {
    let apps = compute_apps_raw()?;
    let me = std::process::id();
    Some(
        apps.into_iter()
            .filter(|a| a.pid == me)
            .map(|a| a.used_mb)
            .sum(),
    )
}

/// Every CUDA process resident on the card. Empty when nvidia-smi is
/// unavailable or nothing is resident.
pub fn compute_apps() -> Vec<ComputeApp> {
    compute_apps_raw().unwrap_or_default()
}

fn compute_apps_raw() -> Option<Vec<ComputeApp>> {
    let stdout = nvidia_smi(&[
        "--query-compute-apps=pid,used_gpu_memory,process_name",
        "--format=csv,noheader,nounits",
    ])?;
    Some(parse_compute_apps(&stdout))
}

fn parse_compute_apps(stdout: &str) -> Vec<ComputeApp> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.split(',').map(str::trim);
            let pid = parts.next()?.parse::<u32>().ok()?;
            let used_mb = parts.next()?.parse::<u64>().ok()?;
            // process_name may itself contain commas; keep the remainder.
            let rest: Vec<&str> = parts.collect();
            let full = rest.join(",");
            let name = full.rsplit('/').next().unwrap_or(&full).trim().to_string();
            Some(ComputeApp { pid, used_mb, name })
        })
        .collect()
}

/// Operator-facing one-liner naming who is holding the card, biggest
/// first: `"pid=613301 7394MB llama-server; pid=588655 8188MB ollama"`.
/// `"none"` when nothing is resident or the card is not visible.
pub fn compute_apps_summary() -> String {
    let mut apps = compute_apps();
    if apps.is_empty() {
        return "none".to_string();
    }
    apps.sort_by_key(|a| std::cmp::Reverse(a.used_mb));
    apps.iter()
        .map(|a| format!("pid={} {}MB {}", a.pid, a.used_mb, a.name))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_gpu_query_line() {
        let info = parse_gpu_info("NVIDIA GeForce RTX 3090, 100, 23097, 1025, 24576\n");
        assert_eq!(info.name.as_deref(), Some("NVIDIA GeForce RTX 3090"));
        assert_eq!(info.utilization_pct, Some(100.0));
        assert_eq!(info.memory_used_mb, Some(23097));
        assert_eq!(info.memory_free_mb, Some(1025));
        assert_eq!(info.memory_total_mb, Some(24576));
    }

    #[test]
    fn empty_output_yields_all_none() {
        assert_eq!(parse_gpu_info("\n"), GpuInfo::default());
    }

    #[test]
    fn parses_compute_apps_and_basenames_process_path() {
        let apps = parse_compute_apps(
            "613301, 7394, /usr/lib/ollama/llama-server\n588655, 8188, /home/x/.local/llama.cpp/cuda-ece963/llama-server\n",
        );
        assert_eq!(
            apps,
            vec![
                ComputeApp {
                    pid: 613301,
                    used_mb: 7394,
                    name: "llama-server".into()
                },
                ComputeApp {
                    pid: 588655,
                    used_mb: 8188,
                    name: "llama-server".into()
                },
            ]
        );
    }

    #[test]
    fn skips_rows_with_unparseable_memory() {
        // `[N/A]` is what nvidia-smi prints for MIG / permission-denied rows.
        let apps = parse_compute_apps("1, [N/A], /usr/bin/x\n2, 512, /usr/bin/y\n");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].pid, 2);
    }
}
