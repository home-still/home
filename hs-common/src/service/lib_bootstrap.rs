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

#[cfg(all(unix, target_os = "linux"))]
fn required_paths() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let cache = home.join(".cache/ort.pyke.io/dfbin");
        if let Ok(platform_rd) = std::fs::read_dir(&cache) {
            for platform in platform_rd.flatten() {
                if let Ok(hash_rd) = std::fs::read_dir(platform.path()) {
                    for hash_dir in hash_rd.flatten() {
                        if hash_dir
                            .path()
                            .join("libonnxruntime_providers_cuda.so")
                            .exists()
                        {
                            out.push(hash_dir.path().to_string_lossy().into_owned());
                        }
                    }
                }
            }
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
    let mut out: Vec<String> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/lib", ".home-still/dyld-libs"] {
            let dir = home.join(rel);
            if dir.join("libpdfium.dylib").exists() {
                out.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    out
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn required_paths() -> Vec<String> {
    Vec::new()
}
