fn main() {
    // Without this, cargo's incremental cache reuses a stale HS_VERSION
    // when CI rebuilds for a new tag (build.rs source unchanged → build
    // script not re-run → rustc-env not re-emitted → dependent crates
    // not invalidated → HS_VERSION baked in at the prior tag).
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    let version = std::env::var("GITHUB_REF_NAME")
        .ok()
        .filter(|v| v.starts_with('v'))
        .map(|v| v.trim_start_matches('v').to_string())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["describe", "--tags", "--always"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().trim_start_matches('v').to_string())
                })
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=HS_VERSION={version}");
}
