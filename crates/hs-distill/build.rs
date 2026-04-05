fn main() {
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
