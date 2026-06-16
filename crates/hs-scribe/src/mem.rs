//! Host memory probe — reads `/proc/meminfo`'s `MemAvailable`. Used as a ceiling
//! so the autotuner won't raise conversion concurrency while the host is already
//! low on RAM (more parallel converts = more concurrent rasterization + VLM
//! working set). Returns `None` on non-Linux or any parse failure, which callers
//! treat as "no ceiling" — the probe never blocks work it cannot measure.

/// Available host memory in MiB, read from `/proc/meminfo`'s `MemAvailable`
/// line. `None` when the file is absent (non-Linux) or the field can't be parsed.
pub fn available_memory_mb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        // Format: "MemAvailable:   12345678 kB"
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}
