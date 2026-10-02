//! The one binary installer shared by `hs upgrade` and `hs mcp install`.
//!
//! Flow for each binary: resolve the release asset and its `.sha256` asset
//! ([`Installer::prepare`]), stream the archive into a private temp file
//! while hashing it, compare the digest, extract the one wanted entry into a
//! uniquely named staged file next to the install location, run the staged
//! binary's `--version` (or scan it for the version string, for the servers
//! that have no such flag), and only then atomically rename it into place.
//! Every error path drops the temp files; the installed binary is untouched
//! until the final rename.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hs_common::reporter::Reporter;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt as _;

pub const DEFAULT_API_BASE: &str = "https://api.github.com/repos/home-still/home";

/// Largest archive accepted from the network.
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Largest binary extracted from an archive.
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
/// Tar headers/padding allowance on top of the binary cap when bounding the
/// decompressed stream (entries that are skipped are decompressed too).
const TAR_OVERHEAD_BYTES: u64 = 16 * 1024 * 1024;
/// Largest `.sha256` asset body.
const MAX_CHECKSUM_BYTES: u64 = 4 * 1024;
const MAX_VERSION_OUTPUT_BYTES: u64 = 64 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const API_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Server binaries that have no `--version` flag (running them with it errors
/// or starts a server); their staged file is scanned for the version string.
const NO_VERSION_FLAG: [&str; 2] = ["hs-scribe-server", "hs-distill-server"];

// ── Release types ───────────────────────────────────────────────

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// A binary resolved against a release, with its expected archive digest.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub binary: String,
    pub tag: String,
    archive_name: String,
    archive_url: String,
    sha256: String,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    max_archive: u64,
    max_binary: u64,
}

pub struct Installer {
    api_base: String,
    target: String,
    token: Option<String>,
    api: reqwest::Client,
    download: reqwest::Client,
    limits: Limits,
}

/// The compile-time platform's release target triple.
pub fn detect_target() -> Result<&'static str> {
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        return Ok("x86_64-apple-darwin");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        return Ok("aarch64-apple-darwin");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        return Ok("x86_64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        return Ok("aarch64-unknown-linux-gnu");
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        return Ok("x86_64-pc-windows-msvc");
    }
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!("Unsupported platform for self-update"))
}

fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

impl Installer {
    /// `api_base` is the repo API root (`https://api.github.com/repos/<o>/<r>`);
    /// it is a parameter so tests can point it at a loopback server.
    pub fn new(api_base: &str, target: &str) -> Result<Self> {
        let ua = format!("hs/{}", env!("HS_VERSION"));
        let build = |total: Duration| -> Result<reqwest::Client> {
            hs_common::http::client_builder()
                .user_agent(ua.clone())
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(total)
                .build()
                .context("failed to build HTTP client")
        };
        Ok(Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            target: target.to_string(),
            token: std::env::var("GITHUB_TOKEN").ok(),
            api: build(API_TIMEOUT)?,
            download: build(DOWNLOAD_TIMEOUT)?,
            limits: Limits {
                max_archive: MAX_ARCHIVE_BYTES,
                max_binary: MAX_BINARY_BYTES,
            },
        })
    }

    #[cfg(all(test, unix))]
    fn with_limits(mut self, max_archive: u64, max_binary: u64) -> Self {
        self.limits = Limits {
            max_archive,
            max_binary,
        };
        self
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    // ── GitHub API ──────────────────────────────────────────────

    async fn api_get(&self, url: &str) -> Result<reqwest::Response> {
        let mut req = self.api.get(url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("Failed to reach GitHub API")?;
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!(
                "GitHub API rate limit exceeded. Set GITHUB_TOKEN env var to authenticate."
            );
        }
        if !resp.status().is_success() {
            anyhow::bail!("GitHub API returned {} for {url}", resp.status());
        }
        Ok(resp)
    }

    pub async fn latest_release(&self) -> Result<Release> {
        self.api_get(&format!("{}/releases/latest", self.api_base))
            .await?
            .json()
            .await
            .context("Failed to parse release JSON")
    }

    /// Highest-semver release among the 10 most recent, pre-releases included.
    pub async fn latest_release_including_pre(&self) -> Result<Release> {
        let mut releases: Vec<Release> = self
            .api_get(&format!("{}/releases?per_page=10", self.api_base))
            .await?
            .json()
            .await
            .context("Failed to parse releases JSON")?;
        // API order is by creation date, not version.
        releases.sort_by_cached_key(|r| {
            std::cmp::Reverse(
                semver::Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name))
                    .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
            )
        });
        releases
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No releases found"))
    }

    pub async fn release_by_tag(&self, tag: &str) -> Result<Release> {
        self.api_get(&format!("{}/releases/tags/{tag}", self.api_base))
            .await?
            .json()
            .await
            .context("Invalid release JSON")
    }

    // ── Resolution ──────────────────────────────────────────────

    pub fn archive_name(&self, binary: &str, tag: &str) -> String {
        let ext = if is_windows_target(&self.target) {
            "zip"
        } else {
            "tar.gz"
        };
        format!("{binary}-{tag}-{}.{ext}", self.target)
    }

    /// Resolve `binary` in `release`. `Ok(None)`: the release publishes no
    /// archive for this platform. An archive without a usable `.sha256`
    /// asset is an error: nothing is ever installed unverified.
    pub async fn prepare(&self, release: &Release, binary: &str) -> Result<Option<Prepared>> {
        let archive_name = self.archive_name(binary, &release.tag_name);
        let Some(asset) = release.asset(&archive_name) else {
            return Ok(None);
        };
        let sha_name = format!("{archive_name}.sha256");
        let sha_asset = release.asset(&sha_name).ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has {archive_name} but no {sha_name}; refusing to install unverified",
                release.tag_name
            )
        })?;
        let resp = self.get_download(&sha_asset.browser_download_url).await?;
        let mut body = Vec::new();
        stream_body(resp, MAX_CHECKSUM_BYTES, &mut body)
            .await
            .with_context(|| format!("failed to fetch {sha_name}"))?;
        let sha256 = parse_checksum(&String::from_utf8_lossy(&body), &archive_name)
            .with_context(|| format!("invalid {sha_name}"))?;
        Ok(Some(Prepared {
            binary: binary.to_string(),
            tag: release.tag_name.clone(),
            archive_name,
            archive_url: asset.browser_download_url.clone(),
            sha256,
        }))
    }

    /// Like [`prepare`](Self::prepare) but a missing archive is an error.
    pub async fn prepare_required(&self, release: &Release, binary: &str) -> Result<Prepared> {
        self.prepare(release, binary).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no {} asset for {}",
                release.tag_name,
                binary,
                self.target
            )
        })
    }

    async fn get_download(&self, url: &str) -> Result<reqwest::Response> {
        let resp = self
            .download
            .get(url)
            .send()
            .await
            .with_context(|| format!("Download failed: {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("Download failed ({}): {url}", resp.status());
        }
        Ok(resp)
    }

    // ── Install ─────────────────────────────────────────────────

    /// Download, verify, stage and atomically install `prepared` at
    /// `install_path`. On any error nothing at `install_path` has changed and
    /// no temp file remains.
    pub async fn install(
        &self,
        prepared: &Prepared,
        install_path: &Path,
        reporter: &Arc<dyn Reporter>,
    ) -> Result<()> {
        let install_dir = install_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Invalid install path {}", install_path.display()))?;
        std::fs::create_dir_all(install_dir)
            .with_context(|| format!("Failed to create {}", install_dir.display()))?;

        reporter.status("Downloading", &prepared.archive_name);
        let resp = self.get_download(&prepared.archive_url).await?;
        // Anonymous private (0600) temp file, removed on drop.
        let mut archive = tempfile::tempfile().context("failed to create temp file")?;
        let digest = stream_body(resp, self.limits.max_archive, &mut archive).await?;

        // INTEGRITY: the expected digest comes from the `.sha256` asset of
        // the same release page. This catches corruption, truncation and CDN
        // tampering; it does NOT protect against a compromised release (an
        // attacker who can replace the archive can replace its checksum).
        // There is no signature infrastructure to verify against. Verified
        // before any extraction.
        if digest != prepared.sha256 {
            anyhow::bail!(
                "checksum mismatch for {}: expected {}, got {digest}",
                prepared.archive_name,
                prepared.sha256
            );
        }
        archive.seek(SeekFrom::Start(0))?;

        let mut staged = tempfile::Builder::new()
            .prefix(&format!(".{}.stage-", prepared.binary))
            .tempfile_in(install_dir)
            .with_context(|| format!("Failed to create temp file in {}", install_dir.display()))?;
        let entry_name = if is_windows_target(&self.target) {
            format!("{}.exe", prepared.binary)
        } else {
            prepared.binary.clone()
        };
        if is_windows_target(&self.target) {
            extract_zip(
                archive,
                &entry_name,
                self.limits.max_binary,
                staged.as_file_mut(),
            )?;
        } else {
            extract_tar_gz(
                archive,
                &entry_name,
                self.limits.max_binary,
                staged.as_file_mut(),
            )?;
        }
        staged.as_file_mut().flush()?;
        staged.as_file().sync_all()?;
        // Close the write handle (exec of a file open for writing is ETXTBSY)
        // while keeping drop-removal of the path.
        let staged = staged.into_temp_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }

        verify_staged_version(&staged, &prepared.binary, &prepared.tag)
            .await
            .with_context(|| {
                format!(
                    "{} from {} failed verification; installed binary left untouched",
                    prepared.binary, prepared.archive_name
                )
            })?;

        replace_file(staged, install_path, cfg!(windows))
            .with_context(|| format!("Failed to replace {}", install_path.display()))
    }
}

// ── Checksum ────────────────────────────────────────────────────

/// Parse `<64 hex>  <archive name>` (one line) and require the exact name.
fn parse_checksum(text: &str, archive_name: &str) -> Result<String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("checksum file is empty"))?;
    if lines.next().is_some() {
        anyhow::bail!("checksum file has more than one line");
    }
    let mut words = line.split_whitespace();
    let (Some(hex), Some(name), None) = (words.next(), words.next(), words.next()) else {
        anyhow::bail!("expected `<sha256>  <archive name>`");
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("`{hex}` is not a 64-digit hex SHA-256");
    }
    if name != archive_name {
        anyhow::bail!("checksum is for `{name}`, not `{archive_name}`");
    }
    Ok(hex.to_ascii_lowercase())
}

/// Stream a response body into `sink`, enforcing `cap` (also against the
/// announced Content-Length), and return the lowercase hex SHA-256.
async fn stream_body(
    mut resp: reqwest::Response,
    cap: u64,
    sink: &mut impl Write,
) -> Result<String> {
    if let Some(len) = resp.content_length() {
        if len > cap {
            anyhow::bail!("download announces {len} bytes, over the {cap}-byte cap");
        }
    }
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("Failed to read download")? {
        total += chunk.len() as u64;
        if total > cap {
            anyhow::bail!("download exceeds the {cap}-byte cap");
        }
        hasher.update(&chunk);
        sink.write_all(&chunk)?;
    }
    sink.flush()?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

// ── Extraction ──────────────────────────────────────────────────

/// Copy `r` to `w`, failing as soon as more than `max` bytes would be written.
fn copy_capped(r: &mut impl Read, w: &mut impl Write, max: u64) -> Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        if total + n as u64 > max {
            anyhow::bail!("archive entry exceeds the {max}-byte cap");
        }
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
}

fn is_wanted(path: &Path, name: &str) -> bool {
    path.file_name().and_then(|f| f.to_str()) == Some(name)
}

/// Extract the regular-file entry named exactly `name` (any directory prefix).
fn extract_tar_gz(archive: impl Read, name: &str, max: u64, out: &mut impl Write) -> Result<()> {
    // Bound the decompressed stream too: skipped entries are decompressed.
    let gz = flate2::read::GzDecoder::new(archive).take(max.saturating_add(TAR_OVERHEAD_BYTES));
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read tar entry")?;
        if !entry.header().entry_type().is_file() || !is_wanted(&entry.path()?, name) {
            continue;
        }
        copy_capped(&mut entry, out, max)?;
        return Ok(());
    }
    anyhow::bail!("Binary '{name}' not found in archive")
}

fn extract_zip(
    archive: impl Read + Seek,
    name: &str,
    max: u64,
    out: &mut impl Write,
) -> Result<()> {
    let mut zip = zip::ZipArchive::new(archive).context("Failed to read zip archive")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("Failed to read zip entry")?;
        if !entry.is_file() || !is_wanted(Path::new(entry.name()), name) {
            continue;
        }
        if entry.size() > max {
            anyhow::bail!("archive entry exceeds the {max}-byte cap");
        }
        copy_capped(&mut entry, out, max)?;
        return Ok(());
    }
    anyhow::bail!("Binary '{name}' not found in archive")
}

// ── Version verification ────────────────────────────────────────

async fn verify_staged_version(staged: &Path, binary: &str, tag: &str) -> Result<()> {
    let want = tag.strip_prefix('v').unwrap_or(tag);
    if NO_VERSION_FLAG.contains(&binary) {
        if !file_contains(staged, want.as_bytes())? {
            anyhow::bail!("{binary} does not embed version {want}");
        }
        return Ok(());
    }

    let mut child = tokio::process::Command::new(staged)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run {} --version", staged.display()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let run = tokio::time::timeout(VERSION_TIMEOUT, async {
        let mut buf = Vec::new();
        (&mut stdout)
            .take(MAX_VERSION_OUTPUT_BYTES)
            .read_to_end(&mut buf)
            .await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((buf, status))
    })
    .await;
    let (buf, status) = match run {
        Ok(r) => r.context("failed to run staged binary")?,
        Err(_) => {
            let _ = child.start_kill();
            anyhow::bail!("`--version` did not finish within {VERSION_TIMEOUT:?}");
        }
    };
    if !status.success() {
        anyhow::bail!("`--version` exited with {status}");
    }
    let out = String::from_utf8_lossy(&buf);
    let got = out.split_whitespace().last().unwrap_or_default();
    if got != want {
        anyhow::bail!("staged binary reports version `{got}`, expected `{want}`");
    }
    Ok(())
}

/// Streaming substring scan with bounded memory and chunk overlap.
fn file_contains(path: &Path, needle: &[u8]) -> Result<bool> {
    const CHUNK: usize = 64 * 1024;
    if needle.is_empty() {
        return Ok(true);
    }
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; CHUNK + needle.len()];
    let mut carry = 0usize;
    loop {
        let n = f.read(&mut buf[carry..carry + CHUNK])?;
        if n == 0 {
            return Ok(false);
        }
        let end = carry + n;
        if buf[..end].windows(needle.len()).any(|w| w == needle) {
            return Ok(true);
        }
        let keep = (needle.len() - 1).min(end);
        buf.copy_within(end - keep..end, 0);
        carry = keep;
    }
}

// ── Replacement ─────────────────────────────────────────────────

fn aside_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    dest.with_file_name(name)
}

/// Rename `staged` over `dest`. With `aside` (Windows, where a running exe
/// cannot be overwritten but can be renamed) the current file is first moved
/// to `<dest>.old` (a stale one from a previous run is removed best effort)
/// and moved back if the final rename fails.
fn replace_file(staged: tempfile::TempPath, dest: &Path, aside: bool) -> Result<()> {
    if aside && dest.exists() {
        let old = aside_path(dest);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dest, &old)
            .with_context(|| format!("could not move {} aside", dest.display()))?;
        if let Err(e) = staged.persist(dest) {
            let _ = std::fs::rename(&old, dest);
            return Err(e.error.into());
        }
        return Ok(());
    }
    staged
        .persist(dest)
        .map_err(|e| anyhow::Error::from(e.error))
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub struct Route {
        pub body: Vec<u8>,
        pub announce_len: Option<usize>,
        pub omit_len: bool,
    }

    impl Route {
        pub fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self {
                body: body.into(),
                announce_len: None,
                omit_len: false,
            }
        }
    }

    /// Loopback HTTP server; returns its base URL (`http://127.0.0.1:port`).
    pub async fn serve(routes: HashMap<String, Route>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = std::sync::Arc::new(routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let line = String::from_utf8_lossy(&req);
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let head;
                    let mut body: &[u8] = &[];
                    match routes.get(&path) {
                        Some(r) => {
                            head = if r.omit_len {
                                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string()
                            } else {
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    r.announce_len.unwrap_or(r.body.len())
                                )
                            };
                            body = &r.body;
                        }
                        None => {
                            head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
                        }
                    }
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_support::{serve, Route};
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const WIN_TARGET: &str = "x86_64-pc-windows-msvc";
    const TAG: &str = "v0.0.1-rc.358";

    /// Serializes installs: exec of a just-written file races with forks in
    /// sibling tests (ETXTBSY) otherwise.
    static EXEC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn reporter() -> Arc<dyn Reporter> {
        Arc::new(hs_common::reporter::SilentReporter)
    }

    fn script(version_out: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"{version_out}\"\n").into_bytes()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            for (name, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_entry_type(tar::EntryType::Regular);
                h.set_cksum();
                b.append_data(&mut h, name, *data).unwrap();
            }
            b.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Fake release: asset routes served under `/dl/<name>` plus API routes.
    struct Fixture {
        routes: HashMap<String, Route>,
        assets: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                routes: HashMap::new(),
                assets: Vec::new(),
            }
        }
        fn add(&mut self, name: &str, body: Route) {
            self.routes.insert(format!("/dl/{name}"), body);
            self.assets.push(name.to_string());
        }
        fn add_archive(&mut self, name: &str, archive: &[u8], sha: Option<&str>) {
            if let Some(sha) = sha {
                self.add(
                    &format!("{name}.sha256"),
                    Route::ok(format!("{sha}  {name}\n")),
                );
            }
            self.add(name, Route::ok(archive));
        }
        async fn start(mut self) -> (String, Release) {
            let base = serve(std::mem::take(&mut self.routes)).await;
            let release = Release {
                tag_name: TAG.to_string(),
                assets: self
                    .assets
                    .iter()
                    .map(|n| Asset {
                        name: n.clone(),
                        browser_download_url: format!("{base}/dl/{n}"),
                    })
                    .collect(),
            };
            (base, release)
        }
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn installed_dir_with_old(dir: &Path) -> PathBuf {
        let p = dir.join("hs");
        std::fs::write(&p, b"OLD").unwrap();
        p
    }

    #[tokio::test]
    async fn checksum_mismatch_installs_nothing_and_leaves_no_temp_files() {
        let _g = EXEC_LOCK.lock().await;
        let archive = tar_gz(&[("hs", &script("hs 0.0.1-rc.358"))]);
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let mut fx = Fixture::new();
        fx.add_archive(&name, &archive, Some(&"0".repeat(64)));
        let (base, release) = fx.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = installed_dir_with_old(dir.path());
        let inst = Installer::new(&base, TARGET).unwrap();
        let prepared = inst.prepare_required(&release, "hs").await.unwrap();
        let err = inst
            .install(&prepared, &dest, &reporter())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"OLD");
        assert_eq!(dir_entries(dir.path()), vec!["hs"]);
    }

    #[tokio::test]
    async fn missing_sha256_asset_is_an_error() {
        let archive = tar_gz(&[("hs", &script("hs 0.0.1-rc.358"))]);
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let mut fx = Fixture::new();
        fx.add_archive(&name, &archive, None);
        let (base, release) = fx.start().await;
        let inst = Installer::new(&base, TARGET).unwrap();
        let err = inst.prepare(&release, "hs").await.unwrap_err();
        assert!(format!("{err:#}").contains(".sha256"), "{err:#}");
    }

    #[tokio::test]
    async fn malformed_or_misnamed_checksum_is_an_error() {
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let mut fx = Fixture::new();
        fx.add(
            &format!("{name}.sha256"),
            Route::ok(format!("{}  other-file.tar.gz\n", "a".repeat(64))),
        );
        fx.add(&name, Route::ok(b"x".to_vec()));
        let (base, release) = fx.start().await;
        let inst = Installer::new(&base, TARGET).unwrap();
        assert!(inst.prepare(&release, "hs").await.is_err());
        assert!(parse_checksum("zz  a", "a").is_err());
        assert!(parse_checksum(&format!("{}  a", "b".repeat(63)), "a").is_err());
    }

    #[tokio::test]
    async fn missing_hs_asset_is_an_error() {
        let fx = Fixture::new();
        let (base, release) = fx.start().await;
        let inst = Installer::new(&base, TARGET).unwrap();
        assert!(inst.prepare(&release, "hs").await.unwrap().is_none());
        let err = inst.prepare_required(&release, "hs").await.unwrap_err();
        assert!(format!("{err:#}").contains("no hs asset"), "{err:#}");
    }

    #[tokio::test]
    async fn version_mismatch_is_rejected_before_rename_and_old_binary_untouched() {
        let _g = EXEC_LOCK.lock().await;
        let archive = tar_gz(&[("hs", &script("hs 0.0.1-rc.1"))]);
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let mut fx = Fixture::new();
        fx.add_archive(&name, &archive, Some(&sha_hex(&archive)));
        let (base, release) = fx.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = installed_dir_with_old(dir.path());
        let inst = Installer::new(&base, TARGET).unwrap();
        let prepared = inst.prepare_required(&release, "hs").await.unwrap();
        let err = inst
            .install(&prepared, &dest, &reporter())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("rc.1"), "{err:#}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"OLD");
        assert_eq!(dir_entries(dir.path()), vec!["hs"]);
    }

    #[tokio::test]
    async fn success_installs_0755_binary_reporting_the_tag_version() {
        let _g = EXEC_LOCK.lock().await;
        let archive = tar_gz(&[("hs", &script("hs 0.0.1-rc.358"))]);
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let mut fx = Fixture::new();
        fx.add_archive(&name, &archive, Some(&sha_hex(&archive)));
        fx.routes.insert(
            "/releases/latest".into(),
            Route::ok(format!(r#"{{"tag_name":"{TAG}","assets":[]}}"#)),
        );
        let (base, release) = fx.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = installed_dir_with_old(dir.path());
        let inst = Installer::new(&base, TARGET).unwrap();
        let prepared = inst.prepare_required(&release, "hs").await.unwrap();
        inst.install(&prepared, &dest, &reporter()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), script("hs 0.0.1-rc.358"));
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let out = std::process::Command::new(&dest)
            .arg("--version")
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            text.split_whitespace().last().unwrap(),
            TAG.trim_start_matches('v')
        );
        assert_eq!(dir_entries(dir.path()), vec!["hs"]);
    }

    #[tokio::test]
    async fn latest_release_is_fetched_from_the_injected_api_base() {
        let mut fx = Fixture::new();
        fx.routes.insert(
            "/releases/latest".into(),
            Route::ok(format!(r#"{{"tag_name":"{TAG}","assets":[]}}"#)),
        );
        fx.routes.insert(
            "/releases?per_page=10".into(),
            Route::ok(
                r#"[{"tag_name":"v0.0.1-rc.9","assets":[]},{"tag_name":"v0.0.1-rc.10","assets":[]}]"#,
            ),
        );
        let (base, _) = fx.start().await;
        let inst = Installer::new(&base, TARGET).unwrap();
        assert_eq!(inst.latest_release().await.unwrap().tag_name, TAG);
        // semver order (rc.10 > rc.9), not API order
        assert_eq!(
            inst.latest_release_including_pre().await.unwrap().tag_name,
            "v0.0.1-rc.10"
        );
    }

    #[test]
    fn decompression_bomb_entry_is_rejected_within_the_cap() {
        struct Counting(u64);
        impl Write for Counting {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0 += b.len() as u64;
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bomb = tar_gz(&[("hs", &vec![0u8; 8 * 1024 * 1024])]);
        assert!(
            bomb.len() < 1024 * 1024 && bomb.len() * 4 < 8 * 1024 * 1024,
            "fixture should compress >4x ({} bytes)",
            bomb.len()
        );
        let cap = 1024 * 1024;
        let mut sink = Counting(0);
        let err = extract_tar_gz(&bomb[..], "hs", cap, &mut sink).unwrap_err();
        assert!(format!("{err:#}").contains("cap"), "{err:#}");
        assert!(sink.0 <= cap, "wrote {} bytes past the cap", sink.0);
    }

    #[tokio::test]
    async fn oversize_download_is_rejected_by_header_and_by_stream() {
        let _g = EXEC_LOCK.lock().await;
        let name = format!("hs-{TAG}-{TARGET}.tar.gz");
        let big = vec![7u8; 64 * 1024];
        for (announce, omit) in [(Some(10 * 1024 * 1024), false), (None, true)] {
            let mut fx = Fixture::new();
            fx.add(
                &format!("{name}.sha256"),
                Route::ok(format!("{}  {name}\n", sha_hex(&big))),
            );
            fx.add(
                &name,
                Route {
                    body: big.clone(),
                    announce_len: announce,
                    omit_len: omit,
                },
            );
            let (base, release) = fx.start().await;
            let dir = tempfile::tempdir().unwrap();
            let dest = installed_dir_with_old(dir.path());
            let inst = Installer::new(&base, TARGET)
                .unwrap()
                .with_limits(8 * 1024, 1 << 20);
            let prepared = inst.prepare_required(&release, "hs").await.unwrap();
            let err = inst
                .install(&prepared, &dest, &reporter())
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("cap"), "{err:#}");
            assert_eq!(dir_entries(dir.path()), vec!["hs"]);
        }
    }

    #[test]
    fn wrong_name_and_non_file_entries_are_not_extracted() {
        let mut sink = Vec::new();
        let a = tar_gz(&[("hs-evil", b"x"), ("dir/hs.sig", b"y")]);
        assert!(extract_tar_gz(&a[..], "hs", 1 << 20, &mut sink).is_err());
        assert!(sink.is_empty());
        // directory entry named like the binary
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "hs/", std::io::empty()).unwrap();
            b.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).unwrap();
        let a = gz.finish().unwrap();
        assert!(extract_tar_gz(&a[..], "hs", 1 << 20, &mut sink).is_err());
        // nested path with the exact file name is accepted
        let a = tar_gz(&[("pkg/hs", b"bin")]);
        extract_tar_gz(&a[..], "hs", 1 << 20, &mut sink).unwrap();
        assert_eq!(sink, b"bin");
    }

    #[test]
    fn zip_extraction_picks_exact_entry_and_enforces_cap() {
        let z = zip_bytes(&[("hs.exe.sig", b"sig"), ("hs.exe", b"MZ-binary")]);
        let mut out = Vec::new();
        extract_zip(std::io::Cursor::new(&z), "hs.exe", 1 << 20, &mut out).unwrap();
        assert_eq!(out, b"MZ-binary");
        let mut out = Vec::new();
        assert!(extract_zip(std::io::Cursor::new(&z), "hs.exe", 4, &mut out).is_err());
        assert!(out.len() <= 4);
        assert!(extract_zip(std::io::Cursor::new(&z), "hs-mcp.exe", 1 << 20, &mut out).is_err());
    }

    #[tokio::test]
    async fn windows_target_installs_from_zip_with_stand_in_exe() {
        let _g = EXEC_LOCK.lock().await;
        let archive = zip_bytes(&[("hs.exe", &script("hs 0.0.1-rc.358"))]);
        let name = format!("hs-{TAG}-{WIN_TARGET}.zip");
        let mut fx = Fixture::new();
        fx.add_archive(&name, &archive, Some(&sha_hex(&archive)));
        let (base, release) = fx.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("hs.exe");
        let inst = Installer::new(&base, WIN_TARGET).unwrap();
        assert_eq!(inst.archive_name("hs", TAG), name);
        let prepared = inst.prepare_required(&release, "hs").await.unwrap();
        inst.install(&prepared, &dest, &reporter()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), script("hs 0.0.1-rc.358"));
    }

    #[test]
    fn rename_aside_replaces_a_busy_file_and_restores_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("hs.exe");
        std::fs::write(&dest, b"OLD").unwrap();
        std::fs::write(dir.path().join("hs.exe.old"), b"STALE").unwrap();
        let staged = tempfile::Builder::new()
            .tempfile_in(dir.path())
            .unwrap()
            .into_temp_path();
        std::fs::write(&staged, b"NEW").unwrap();
        replace_file(staged, &dest, true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW");
        assert_eq!(
            std::fs::read(dir.path().join("hs.exe.old")).unwrap(),
            b"OLD"
        );

        // persist failure (destination is a non-empty dir): old file restored
        let dest2 = dir.path().join("hs2.exe");
        std::fs::write(&dest2, b"OLD2").unwrap();
        let staged = tempfile::Builder::new()
            .tempfile_in(dir.path())
            .unwrap()
            .into_temp_path();
        std::fs::remove_file(&staged).unwrap(); // persist will fail: source gone
        assert!(replace_file(staged, &dest2, true).is_err());
        assert_eq!(std::fs::read(&dest2).unwrap(), b"OLD2");
    }

    #[test]
    fn embedded_version_scan_spans_chunk_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let needle = b"0.0.1-rc.358";
        for offset in [0usize, 65530, 65536 - 5, 131072 - 3, 200_000] {
            let mut data = vec![b'x'; 300_000];
            data[offset..offset + needle.len()].copy_from_slice(needle);
            let p = dir.path().join("bin");
            std::fs::write(&p, &data).unwrap();
            assert!(file_contains(&p, needle).unwrap(), "offset {offset}");
        }
        let p = dir.path().join("none");
        std::fs::write(&p, vec![b'x'; 300_000]).unwrap();
        assert!(!file_contains(&p, needle).unwrap());
    }

    #[tokio::test]
    async fn server_binaries_are_checked_by_embedded_version_not_exec() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hs-distill-server");
        std::fs::write(&p, b"\x7fELF....0.0.1-rc.358....").unwrap();
        verify_staged_version(&p, "hs-distill-server", TAG)
            .await
            .unwrap();
        std::fs::write(&p, b"\x7fELF....0.0.1-rc.357....").unwrap();
        assert!(verify_staged_version(&p, "hs-distill-server", TAG)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn hanging_version_probe_is_killed_after_timeout() {
        let _g = EXEC_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hs");
        std::fs::write(&p, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // paused clock: auto-advances to the 10 s timeout once the runtime is idle
        tokio::time::pause();
        let err = verify_staged_version(&p, "hs", TAG).await.unwrap_err();
        assert!(format!("{err:#}").contains("did not finish"), "{err:#}");
    }
}
