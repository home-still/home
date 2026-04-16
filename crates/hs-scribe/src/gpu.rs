/// Shell out to nvidia-smi once and parse the first GPU's name, utilization %, and memory used in MiB.
/// Returns all `None` when nvidia-smi is absent or fails, so non-GPU hosts keep a clean health response.
pub fn query_gpu_info() -> (Option<String>, Option<f32>, Option<u64>) {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok();

    let Some(o) = output else {
        return (None, None, None);
    };
    if !o.status.success() {
        return (None, None, None);
    }

    let stdout = String::from_utf8_lossy(&o.stdout);
    let line = stdout.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return (None, None, None);
    }

    let mut parts = line.split(',').map(str::trim);
    let name = parts.next().filter(|s| !s.is_empty()).map(String::from);
    let util = parts.next().and_then(|s| s.parse::<f32>().ok());
    let mem_mb = parts.next().and_then(|s| s.parse::<u64>().ok());

    (name, util, mem_mb)
}
