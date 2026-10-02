use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

#[cfg(feature = "storage-s3")]
pub mod s3;

#[cfg(feature = "storage-s3")]
pub use s3::S3Storage;

pub mod config;
pub use config::{Backend, StorageConfig};

/// Walk the cause chain of `err` and return true if any wrapped error
/// indicates "object not found in storage." Used by event handlers to
/// classify a `get(...) failed` as Permanent — the bytes don't exist
/// and re-delivering the event will never make them appear.
///
/// Recognizes both backends:
/// - S3 / object_store: `object_store::Error::NotFound { .. }`
/// - LocalFs: `std::io::Error` with `ErrorKind::NotFound`
pub fn is_not_found(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        #[cfg(feature = "storage-s3")]
        if let Some(oe) = cause.downcast_ref::<object_store::Error>() {
            if matches!(oe, object_store::Error::NotFound { .. }) {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::NotFound {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<SystemTime>,
    pub etag: Option<String>,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()>;
    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.head(key).await?.is_some())
    }

    /// Provision any container the backend needs before writes succeed
    /// (e.g. an S3 bucket). Default is a noop — only S3 overrides.
    /// Idempotent; callers can call on every startup.
    async fn ensure_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A storage key or prefix that [`validate_key`] / [`validate_prefix`]
/// rejected. Distinct from "not found" and from transport errors, so event
/// handlers can classify it as a permanent (non-retryable) failure via
/// [`is_invalid_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidKey {
    pub key: String,
    pub reason: &'static str,
}

impl std::fmt::Display for InvalidKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid storage key {:?}: {}", self.key, self.reason)
    }
}

impl std::error::Error for InvalidKey {}

/// True if `err` (or any cause) is an [`InvalidKey`].
pub fn is_invalid_key(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<InvalidKey>())
}

/// Reject storage keys that could address anything outside the storage root:
/// the empty key, NUL, `\`, absolute keys (a leading `/`, or a drive/UNC
/// prefix on Windows) and `.` / `..` segments.
///
/// The single definition of a valid key, enforced by every [`Storage`]
/// backend on every operation. Callers that build keys from untrusted text
/// (event payloads, MCP arguments, file names) get the same `Err` earlier by
/// calling this, or [`crate::validate_stem`] for a bare stem.
pub fn validate_key(key: &str) -> Result<(), InvalidKey> {
    if key.is_empty() {
        return Err(InvalidKey {
            key: String::new(),
            reason: "empty",
        });
    }
    check_key_text(key)
}

/// Like [`validate_key`] for a `list` prefix: the empty prefix (everything)
/// and a trailing `/` are allowed.
pub fn validate_prefix(prefix: &str) -> Result<(), InvalidKey> {
    check_key_text(prefix)
}

fn check_key_text(text: &str) -> Result<(), InvalidKey> {
    let reason = if text.contains('\0') {
        "contains NUL"
    } else if text.contains('\\') {
        "contains a backslash"
    } else if text.starts_with('/')
        || Path::new(text)
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        "absolute path"
    } else if text.split('/').any(|seg| seg == "." || seg == "..") {
        "'.' or '..' segment"
    } else {
        return Ok(());
    };
    Err(InvalidKey {
        key: text.to_string(),
        reason,
    })
}

pub struct LocalFsStorage {
    root: PathBuf,
}

/// Temp files written by [`LocalFsStorage::put`] before the rename that
/// publishes them. Hidden from `list`.
const PUT_TEMP_PREFIX: &str = ".hs-put-";
const PUT_TEMP_SUFFIX: &str = ".tmp";
static PUT_SEQ: AtomicU64 = AtomicU64::new(0);

fn is_put_temp(file_name: &std::ffi::OsStr) -> bool {
    file_name
        .to_str()
        .is_some_and(|n| n.starts_with(PUT_TEMP_PREFIX) && n.ends_with(PUT_TEMP_SUFFIX))
}

/// A unique sibling of `path` for the write-then-rename in `put`. The name
/// does not embed the target's name, so it cannot overflow NAME_MAX when the
/// target itself fits.
fn put_temp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = PUT_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        "{PUT_TEMP_PREFIX}{}-{nanos:x}-{seq}{PUT_TEMP_SUFFIX}",
        std::process::id()
    ))
}

impl LocalFsStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Map a key to a path under `root`. The only place a key becomes a
    /// filesystem path: every trait method goes through it.
    fn resolve(&self, key: &str) -> anyhow::Result<PathBuf> {
        validate_key(key)?;
        check_local_name(key)?;
        Ok(self.root.join(key))
    }

    fn resolve_prefix(&self, prefix: &str) -> anyhow::Result<PathBuf> {
        validate_prefix(prefix)?;
        check_local_name(prefix)?;
        Ok(self.root.join(prefix))
    }

    fn key_from(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }
}

/// Win32 silently strips a trailing '.' or ' ' from every path segment, so on
/// Windows such a key would name a different file (or fail with access
/// denied). Refused there as an invalid key; other platforms store it as is.
fn check_local_name(key: &str) -> Result<(), InvalidKey> {
    if cfg!(windows)
        && key
            .split('/')
            .any(|seg| seg.ends_with('.') || seg.ends_with(' '))
    {
        return Err(InvalidKey {
            key: key.to_string(),
            reason: "a segment ends with '.' or ' ' (not storable on Windows)",
        });
    }
    Ok(())
}

/// Write `bytes` to `tmp`, fsync it, then rename it over `path`. Readers see
/// either the old object or the complete new one, never a prefix of it.
async fn write_then_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(tmp, path).await
}

#[async_trait]
impl Storage for LocalFsStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = self.resolve(key)?;
        Ok(tokio::fs::read(&path).await?)
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = put_temp_path(&path);
        if let Err(e) = write_then_rename(&tmp, &path, &bytes).await {
            // Best effort: the original error is the one worth reporting.
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        Ok(())
    }

    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        let path = self.resolve(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(md) if md.is_file() => Ok(Some(ObjectMeta {
                key: key.to_string(),
                size: md.len(),
                last_modified: md.modified().ok(),
                etag: None,
            })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let start = self.resolve_prefix(prefix)?;
        let mut out = Vec::new();
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() && !is_put_temp(&entry.file_name()) {
                    let md = entry.metadata().await?;
                    if let Some(key) = self.key_from(&path) {
                        out.push(ObjectMeta {
                            key,
                            size: md.len(),
                            last_modified: md.modified().ok(),
                            etag: None,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());

        s.put("a/b/x.txt", b"hello".to_vec()).await.unwrap();
        assert_eq!(s.get("a/b/x.txt").await.unwrap(), b"hello");

        let meta = s.head("a/b/x.txt").await.unwrap().unwrap();
        assert_eq!(meta.size, 5);
        assert_eq!(meta.key, "a/b/x.txt");

        assert!(s.exists("a/b/x.txt").await.unwrap());
        assert!(!s.exists("a/b/missing.txt").await.unwrap());

        s.put("a/c/y.txt", b"yo".to_vec()).await.unwrap();
        let mut list = s.list("a").await.unwrap();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].key, "a/b/x.txt");
        assert_eq!(list[1].key, "a/c/y.txt");

        s.delete("a/b/x.txt").await.unwrap();
        assert!(!s.exists("a/b/x.txt").await.unwrap());
        s.delete("a/b/x.txt").await.unwrap();
    }

    #[tokio::test]
    async fn is_not_found_recognises_local_fs_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());
        let err = s.get("does/not/exist.bin").await.unwrap_err();
        assert!(is_not_found(&err), "expected NotFound, got: {err:#}");
    }

    #[test]
    fn is_not_found_walks_cause_chain() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let wrapped = anyhow::Error::from(io).context("get(papers/x.html) failed");
        assert!(is_not_found(&wrapped));
    }

    #[test]
    fn is_not_found_rejects_unrelated_errors() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let wrapped = anyhow::Error::from(io);
        assert!(!is_not_found(&wrapped));
    }

    const ESCAPING_KEYS: &[&str] = &[
        "",
        ".",
        "..",
        "../escape.txt",
        "a/../../escape.txt",
        "a/..",
        "a/./b",
        "/etc/passwd",
        "/abs.txt",
        "a\\b.txt",
        "..\\escape.txt",
        "a\0b.txt",
    ];

    #[tokio::test]
    async fn local_fs_rejects_escaping_keys_on_every_operation() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outer.path().join("escape.txt"), b"outside").unwrap();
        let s = LocalFsStorage::new(&root);

        for key in ESCAPING_KEYS {
            let put = s.put(key, b"x".to_vec()).await.unwrap_err();
            assert!(is_invalid_key(&put), "put({key:?}): {put:#}");
            assert!(
                is_invalid_key(&s.get(key).await.unwrap_err()),
                "get({key:?})"
            );
            assert!(
                is_invalid_key(&s.head(key).await.unwrap_err()),
                "head({key:?})"
            );
            assert!(
                is_invalid_key(&s.delete(key).await.unwrap_err()),
                "delete({key:?})"
            );
            assert!(
                is_invalid_key(&s.exists(key).await.unwrap_err()),
                "exists({key:?})"
            );
        }

        // Nothing was created, read or deleted outside (or inside) the root.
        assert_eq!(
            std::fs::read(outer.path().join("escape.txt")).unwrap(),
            b"outside"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn local_fs_list_rejects_escaping_prefixes_but_allows_root_and_trailing_slash() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outer.path().join("secret.txt"), b"outside").unwrap();
        let s = LocalFsStorage::new(&root);
        s.put("a/x.txt", b"x".to_vec()).await.unwrap();

        for prefix in ["..", "../", "a/../..", "/", "/etc", "a\\b", "a\0"] {
            let err = s.list(prefix).await.unwrap_err();
            assert!(is_invalid_key(&err), "list({prefix:?}): {err:#}");
        }

        let all = s.list("").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].key, "a/x.txt");
        assert_eq!(s.list("a/").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn local_fs_accepts_dots_and_percent_inside_names() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());
        for key in ["a/..b/c..d.md", "a/foo%3Cbar.md", "a/.hidden"] {
            s.put(key, b"ok".to_vec()).await.unwrap();
            assert_eq!(s.get(key).await.unwrap(), b"ok", "{key}");
        }
        // A trailing '.' is a valid name everywhere but Windows (Win32 strips it).
        let trailing = s.put("a/...", b"ok".to_vec()).await;
        if cfg!(windows) {
            assert!(is_invalid_key(&trailing.unwrap_err()));
        } else {
            trailing.unwrap();
            assert_eq!(s.get("a/...").await.unwrap(), b"ok");
        }
    }

    #[test]
    fn invalid_key_is_not_not_found() {
        let err = anyhow::Error::from(validate_key("../x").unwrap_err());
        assert!(is_invalid_key(&err));
        assert!(!is_not_found(&err));
        assert!(!is_invalid_key(&anyhow::anyhow!("unrelated")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_fs_put_is_atomic_for_concurrent_readers() {
        let tmp = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(LocalFsStorage::new(tmp.path()));
        let a = vec![b'a'; 2 * 1024 * 1024];
        let b = vec![b'b'; 3 * 1024 * 1024];
        s.put("m/doc.md", a.clone()).await.unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..3 {
            let s = s.clone();
            let stop = stop.clone();
            let (a_len, b_len) = (a.len(), b.len());
            readers.push(tokio::spawn(async move {
                let mut reads = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let got = s.get("m/doc.md").await.unwrap();
                    let first = got[0];
                    let want = if first == b'a' { a_len } else { b_len };
                    assert_eq!(got.len(), want, "truncated or mixed read");
                    assert!(got.iter().all(|&c| c == first), "mixed read");
                    reads += 1;
                }
                reads
            }));
        }
        for i in 0..40 {
            let body = if i % 2 == 0 { b.clone() } else { a.clone() };
            s.put("m/doc.md", body).await.unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            assert!(r.await.unwrap() > 0);
        }
    }

    #[tokio::test]
    async fn local_fs_put_leaves_no_temp_file_and_hides_stale_ones_from_list() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());
        s.put("d/one.md", b"1".to_vec()).await.unwrap();
        s.put("d/one.md", b"22".to_vec()).await.unwrap();

        // A temp left by a crashed writer must not surface as an object.
        std::fs::write(tmp.path().join("d/.hs-put-1-2-3.tmp"), b"partial").unwrap();

        let names: Vec<_> = std::fs::read_dir(tmp.path().join("d"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let listed = s.list("d").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "d/one.md");
        assert_eq!(s.get("d/one.md").await.unwrap(), b"22");
    }

    #[tokio::test]
    async fn local_fs_failed_put_keeps_old_object_and_cleans_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());
        // `d/dir` is a directory, so the final rename onto it fails.
        std::fs::create_dir_all(tmp.path().join("d/dir")).unwrap();
        s.put("d/dir", b"x".to_vec()).await.unwrap_err();
        let names: Vec<_> = std::fs::read_dir(tmp.path().join("d"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["dir".to_string()]);
    }
}
