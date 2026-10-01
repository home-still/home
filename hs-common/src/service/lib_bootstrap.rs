//! Make GPU-accelerated servers (`hs-scribe-server`, `hs-distill-server`)
//! self-hosting w.r.t. the dynamic-library search path.
//!
//! Two host-class concerns collapse into one trampoline:
//!
//! **Linux / CUDA.** ort 2.0.0-rc.11 statically links `libonnxruntime.a` from
//! the pyke cache but `dlopen`s the CUDA provider (unqualified name
//! `libonnxruntime_providers_cuda.so`). The loader's default search finds the
//! Arch package `/usr/lib/libonnxruntime_providers_cuda.so` (1.24.4) before
//! the pyke cache — ABI-mismatched, segfault in provider init. Additionally,
//! the pyke bundle ships `cu12` providers that need `libcublas.so.12` /
//! `libcudart.so.12` / `libcufft.so.11`, which a CUDA-13-only host lacks.
//! Runtime-only wheels from NVIDIA drop those into
//! `~/.home-still/cuda12-libs/`.
//!
//! **macOS / pdfium.** The pdfium-render crate `dlopen`s `libpdfium.dylib`
//! at runtime. macOS doesn't have a system pdfium, and Homebrew doesn't
//! ship it. Deployments drop the bblanchon prebuilt into `~/.local/lib/`
//! (or `~/.home-still/dyld-libs/`). Without `DYLD_LIBRARY_PATH` set, the
//! dlopen returns `image not found` and scribe panics on the first PDF.
//!
//! **The fix.** Call [`ensure_lib_paths_or_reexec`] at the top of `main`.
//! It prepends the discovered dirs to `LD_LIBRARY_PATH` (Linux) or
//! `DYLD_LIBRARY_PATH` (macOS) and re-execs self. `HS_LIB_BOOTSTRAPPED`
//! guards against an exec loop.
//!
//! Unix-only (uses `exec`). On non-Unix the function is a no-op.

#[cfg(unix)]
const PATH_VAR: &str = if cfg!(target_os = "macos") {
    "DYLD_LIBRARY_PATH"
} else {
    "LD_LIBRARY_PATH"
};

#[cfg(unix)]
pub fn ensure_lib_paths_or_reexec() {
    if std::env::var_os("HS_LIB_BOOTSTRAPPED").is_some() {
        return;
    }
    let needed = required_paths();
    if needed.is_empty() {
        return;
    }
    let current = std::env::var(PATH_VAR).unwrap_or_default();
    let has_all = needed
        .iter()
        .all(|p| current.split(':').any(|seg| seg == p.as_str()));
    if has_all {
        return;
    }

    let mut merged = String::new();
    for p in &needed {
        if !merged.is_empty() {
            merged.push(':');
        }
        merged.push_str(p);
    }
    if !current.is_empty() {
        merged.push(':');
        merged.push_str(&current);
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hs-lib-bootstrap: current_exe failed: {e}");
            return;
        }
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&exe)
        .args(&args)
        .env(PATH_VAR, &merged)
        .env("HS_LIB_BOOTSTRAPPED", "1")
        .exec();
    eprintln!("hs-lib-bootstrap: re-exec failed: {err}");
    std::process::exit(127);
}

#[cfg(not(unix))]
pub fn ensure_lib_paths_or_reexec() {}

/// Directories a deployment drops `libpdfium` into besides the system
/// search path. [`ensure_lib_paths_or_reexec`] puts them on the loader path
/// of the GPU servers (macOS); processes that cannot re-exec themselves
/// (the `hs` CLI and its watchers) bind the library from these directories
/// directly. Only directories under the home directory; empty without one.
pub fn pdfium_drop_dirs() -> Vec<std::path::PathBuf> {
    dirs::home_dir()
        .map(|home| {
            [".local/lib", ".home-still/dyld-libs"]
                .iter()
                .map(|rel| home.join(rel))
                .collect()
        })
        .unwrap_or_default()
}

/// Name the pyke cache uses for this platform's directory.
#[cfg(all(unix, target_os = "linux"))]
fn pyke_platform_dir() -> String {
    format!("{}-unknown-linux-gnu", std::env::consts::ARCH)
}

/// The pyke `dfbin/<platform>/<hash>` directory whose
/// `libonnxruntime_providers_cuda.so` the loader must find, plus every other
/// candidate that was passed over.
///
/// The cache gains a new hash directory each time the ort version or feature
/// set changes and never prunes the old ones, so a host that has built more
/// than one configuration holds several ABI-incompatible providers. Putting
/// them all on `LD_LIBRARY_PATH` in `read_dir` order left the winner to the
/// filesystem. Exactly one directory is chosen, by a fixed rule: the oldest
/// provider (earliest mtime, ties broken by hash). The long-lived deployed
/// bundle is the oldest one on a host that accumulated experiments
/// afterwards; and it is the one a first-in-path-wins `read_dir` order
/// picked on the one multi-bundle host this was verified on. Operators
/// should prune bundles no deployed binary links.
#[cfg(all(unix, target_os = "linux"))]
fn pick_pyke_cuda_dir(
    cache: &std::path::Path,
    platform: &str,
) -> Option<(std::path::PathBuf, Vec<std::path::PathBuf>)> {
    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for hash_dir in std::fs::read_dir(cache.join(platform)).ok()?.flatten() {
        let provider = hash_dir.path().join("libonnxruntime_providers_cuda.so");
        if let Ok(meta) = std::fs::metadata(&provider) {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            found.push((mtime, hash_dir.path()));
        }
    }
    found.sort();
    let mut dirs = found.into_iter().map(|(_, d)| d);
    let chosen = dirs.next()?;
    Some((chosen, dirs.collect()))
}

#[cfg(all(unix, target_os = "linux"))]
fn required_paths() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let cache = home.join(".cache/ort.pyke.io/dfbin");
        if let Some((chosen, skipped)) = pick_pyke_cuda_dir(&cache, &pyke_platform_dir()) {
            if !skipped.is_empty() {
                eprintln!(
                    "hs-lib-bootstrap: {} pyke CUDA bundles in {}; using {} (oldest). \
                     Prune bundles no deployed binary links.",
                    skipped.len() + 1,
                    cache.display(),
                    chosen.display()
                );
            }
            out.push(chosen.to_string_lossy().into_owned());
        }

        let cuda12 = home.join(".home-still/cuda12-libs");
        if cuda12.exists() {
            out.push(cuda12.to_string_lossy().into_owned());
        }
    }

    for extra in [
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
        "/usr/local/lib",
    ] {
        if std::path::Path::new(extra).exists() {
            out.push(extra.to_string());
        }
    }

    out
}

#[cfg(all(unix, target_os = "macos"))]
fn required_paths() -> Vec<String> {
    // macOS: scribe-server dlopens libpdfium.dylib. Two standard drop
    // locations are searched — whichever contains the dylib gets added.
    // Other dylibs bundled in the same dir ride along for free.
    pdfium_drop_dirs()
        .into_iter()
        .filter(|dir| dir.join("libpdfium.dylib").exists())
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn required_paths() -> Vec<String> {
    Vec::new()
}

#[cfg(all(test, unix, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn bundle(cache: &std::path::Path, platform: &str, hash: &str, age_secs: u64, cuda: bool) {
        let dir = cache.join(platform).join(hash);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libonnxruntime.a"), b"static").unwrap();
        if cuda {
            let p = dir.join("libonnxruntime_providers_cuda.so");
            std::fs::write(&p, b"provider").unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
                .unwrap();
        }
    }

    #[test]
    fn single_bundle_is_chosen_without_skipped_candidates() {
        let cache = tempfile::tempdir().unwrap();
        bundle(cache.path(), "x86_64-unknown-linux-gnu", "aaa", 10, true);
        bundle(
            cache.path(),
            "x86_64-unknown-linux-gnu",
            "cpuonly",
            5,
            false,
        );
        let (chosen, skipped) =
            pick_pyke_cuda_dir(cache.path(), "x86_64-unknown-linux-gnu").unwrap();
        assert!(chosen.ends_with("aaa"));
        assert!(skipped.is_empty());
    }

    /// RA-82: with several CUDA bundles the choice must not depend on
    /// `read_dir` order — the same set always yields the same winner.
    #[test]
    fn several_bundles_resolve_to_the_oldest_independent_of_creation_order() {
        let platform = "x86_64-unknown-linux-gnu";
        let orders: [[(&str, u64); 3]; 3] = [
            [("d3c0", 500_000), ("6e78", 400_000), ("b08e", 100)],
            [("b08e", 100), ("6e78", 400_000), ("d3c0", 500_000)],
            [("6e78", 400_000), ("b08e", 100), ("d3c0", 500_000)],
        ];
        for order in orders {
            let cache = tempfile::tempdir().unwrap();
            for (hash, age) in order {
                bundle(cache.path(), platform, hash, age, true);
            }
            let (chosen, skipped) = pick_pyke_cuda_dir(cache.path(), platform).unwrap();
            assert!(chosen.ends_with("d3c0"), "{chosen:?}");
            assert_eq!(skipped.len(), 2);
        }
    }

    #[test]
    fn equal_mtimes_fall_back_to_hash_order() {
        let platform = "x86_64-unknown-linux-gnu";
        let cache = tempfile::tempdir().unwrap();
        for hash in ["ccc", "aaa", "bbb"] {
            bundle(cache.path(), platform, hash, 0, true);
            let p = cache
                .path()
                .join(platform)
                .join(hash)
                .join("libonnxruntime_providers_cuda.so");
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000))
                .unwrap();
        }
        let (chosen, _) = pick_pyke_cuda_dir(cache.path(), platform).unwrap();
        assert!(chosen.ends_with("aaa"), "{chosen:?}");
    }

    /// Another platform's bundles (a cache shared across architectures) are
    /// never candidates.
    #[test]
    fn other_platforms_are_ignored() {
        let cache = tempfile::tempdir().unwrap();
        bundle(
            cache.path(),
            "aarch64-unknown-linux-gnu",
            "arm",
            999_999,
            true,
        );
        assert!(pick_pyke_cuda_dir(cache.path(), "x86_64-unknown-linux-gnu").is_none());
        bundle(cache.path(), "x86_64-unknown-linux-gnu", "x86", 1, true);
        let (chosen, _) = pick_pyke_cuda_dir(cache.path(), "x86_64-unknown-linux-gnu").unwrap();
        assert!(chosen.ends_with("x86"));
    }
}
