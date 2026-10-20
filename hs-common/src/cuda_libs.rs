//! The loader path a CUDA-backed service needs.
//!
//! The ONNX Runtime CUDA provider is a shared library that is loaded at
//! runtime, and it needs the CUDA runtime libraries *next to it* to
//! load at all. On a host whose CUDA toolkit is version 13, the
//! provider — which wants `libcublasLt.so.12`, `libcudart.so.12` and
//! `libcufft.so.11` — cannot find them: the system has `.so.13`.
//!
//! The fix is to put the CUDA 12 libraries on the loader path. They
//! are already on the host, in two places:
//!
//! - `~/.local/lib`, where the NVIDIA runtime-only wheels are
//!   installed, and
//! - `~/.home-still/cuda12-libs`, a home-still-managed compat
//!   directory for the same libraries.
//!
//! The ONNX Runtime provider itself lives in the pyke cache
//! (`~/.cache/ort.pyke.io/dfbin`), and that directory has to be on the
//! path too, or the provider is not found at all.
//!
//! **One path, for every service.** The distill server and the scribe
//! server both load the CUDA provider, so they both get their loader
//! path from this module. When one of them had it and the other did
//! not, the scribe server crash-looped on a host whose CUDA toolkit
//! had been upgraded, while the distill server was fine: the same
//! library, the same host, one working and one not, because the path
//! was set in one place and not the other.
//!
//! The path is only extended, never replaced: whatever the caller
//! already had keeps its entries, so a caller that needs something
//! else on the path still has it.

use std::path::PathBuf;

/// The directories searched for the ONNX Runtime CUDA provider.
///
/// Returns the platform-specific directory that actually holds
/// `libonnxruntime_providers_cuda.so`, or `None` when the pyke cache
/// has not been populated on this host.
pub fn ort_cuda_libs_dir() -> Option<PathBuf> {
    let cache = dirs::home_dir()?.join(".cache/ort.pyke.io/dfbin");
    if !cache.exists() {
        return None;
    }
    // Walk into the platform-specific subdir to find the provider.
    for entry in std::fs::read_dir(&cache).ok()?.flatten() {
        let platform_dir = entry.path();
        // A stray file beside the platform dirs is not a reason to stop
        // looking; only an unreadable entry is skipped.
        let Ok(hash_entries) = std::fs::read_dir(&platform_dir) else {
            continue;
        };
        for hash_entry in hash_entries.flatten() {
            let dir = hash_entry.path();
            if dir.join("libonnxruntime_providers_cuda.so").exists() {
                return Some(dir);
            }
        }
    }
    None
}

/// The separator between the entries of a loader path.
///
/// `:` on Unix, `;` on Windows. Getting this wrong is not a cosmetic
/// problem: a Windows loader path joined with `:` is one malformed
/// entry, and every library in it goes unfound.
const PATH_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// The loader path a CUDA-backed service needs.
///
/// The result is the caller's existing `LD_LIBRARY_PATH` with the
/// directories a CUDA provider needs appended, in this order:
///
/// 1. the ONNX Runtime provider's own directory (the pyke cache);
/// 2. the standard CUDA locations;
/// 3. `~/.local/lib` and `~/.home-still/cuda12-libs`, which carry the
///    CUDA 12 libraries a host with a CUDA 13 toolkit lacks.
pub fn cuda_ld_library_path(existing: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Whatever the caller already had comes first: it is the caller's
    // own configuration and this module only adds to it.
    for entry in existing.split(PATH_SEPARATOR) {
        if !entry.is_empty() {
            parts.push(entry.to_string());
        }
    }

    if let Some(dir) = ort_cuda_libs_dir() {
        push_unique(&mut parts, &dir.to_string_lossy());
    }
    for extra in [
        "/usr/local/lib",
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
    ] {
        push_unique(&mut parts, extra);
    }
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/lib", ".home-still/cuda12-libs"] {
            push_unique(&mut parts, &home.join(rel).to_string_lossy());
        }
    }

    parts.join(PATH_SEPARATOR)
}

/// Add a directory to the path, unless it is already there.
fn push_unique(parts: &mut Vec<String>, dir: &str) {
    if !dir.is_empty() && !parts.iter().any(|p| p == dir) {
        parts.push(dir.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_keeps_what_the_caller_already_had() {
        let path = cuda_ld_library_path("/opt/my/own/lib");
        assert!(
            path.starts_with("/opt/my/own/lib:"),
            "the caller's own path must come first: {path}"
        );
    }

    #[test]
    fn the_path_carries_the_cuda_12_compat_directories() {
        let path = cuda_ld_library_path("");
        // The CUDA-12 libraries live in these two, and a host with a
        // CUDA 13 toolkit has no other source for them.
        assert!(path.contains(".local/lib"), "{path}");
        assert!(path.contains("cuda12-libs"), "{path}");
    }

    #[test]
    fn the_path_has_no_repeated_directory() {
        let path = cuda_ld_library_path("/usr/local/lib");
        let mut seen = std::collections::HashSet::new();
        for part in path.split(PATH_SEPARATOR) {
            assert!(seen.insert(part.to_string()), "repeated {part} in {path}");
        }
    }

    #[test]
    fn the_path_is_joined_with_the_platform_separator() {
        // A Windows loader path joined with `:` is one malformed entry,
        // and every library in it goes unfound.
        let path = cuda_ld_library_path("/one");
        assert!(
            path.contains(if cfg!(windows) { ";" } else { ":" }),
            "{path}"
        );
        assert!(path.starts_with("/one"), "{path}");
    }

    #[test]
    fn the_provider_directory_is_found_when_the_cache_is_populated() {
        // The function is a pure lookup: it either finds a directory
        // holding the provider, or there is none on this host.
        if let Some(dir) = ort_cuda_libs_dir() {
            assert!(dir.join("libonnxruntime_providers_cuda.so").exists());
        }
    }
}
